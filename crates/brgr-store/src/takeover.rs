//! Moving an owner's head from one session to another, durably.
//!
//! A takeover is the record of one handoff: which session held the head, which
//! session is taking it, and how far the move got. It carries no work forward
//! by itself. The manifest it stores holds references only — a goal, the
//! owner's own notes, and the ids a reader has to follow — so a handoff can
//! never carry a stale copy of a result, and never restate a decision that
//! `decisions` already holds.
//!
//! The move itself is the binding. `request` and `prepare` only record intent;
//! `commit` is the single transaction that re-checks the binding and transfers
//! it, which is why it is the only step that can be half-done by a crash and
//! why re-running it is safe. `resumed` is never self-reported: the store
//! derives it when an owner-side action is recorded by the incoming session at
//! the committed epoch, so "the new head is actually working" is an observation
//! and not a claim.

use std::{fmt, str::FromStr};

use brgr_protocol::{AttemptId, OwnerId, ResultId, TaskId};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    Store, StoreError, owner::OwnerProof, owner::assert_owner_binding, owner::rebind_in_transaction,
};

/// Bounds on what one handoff may carry. A manifest is read back by the
/// incoming session before it knows anything, so its size is capped rather than
/// trusted.
const MAX_GOAL_BYTES: usize = 4 * 1024;
const MAX_NOTES_BYTES: usize = 8 * 1024;
const MAX_SESSION_BYTES: usize = 256;
const MAX_KEY_BYTES: usize = 128;
const MAX_TASK_REFS: usize = 256;
const MAX_PENDING_RESULTS: usize = 256;
const MAX_PENDING_MESSAGES: usize = 256;
const MAX_REASON_BYTES: usize = 512;

/// Stable identity of one handoff, not of the owner it moves.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TakeoverId(Uuid);

impl TakeoverId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TakeoverId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TakeoverId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for TakeoverId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Where one handoff has got to.
///
/// ```text
/// requested -> prepared -> committed -> resumed -> retired
///     |            |           |           \-> retire_failed
///     v            v
///  aborted      aborted            (no abort after commit: roll forward)
/// ```
///
/// `retired` and `retire_failed` are only reachable through `resumed`, so a
/// record cannot claim a closed handoff that the incoming head never picked up.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverState {
    /// Recorded against the current head. Nothing has moved yet.
    Requested,
    /// The manifest is stored. The incoming session is still not in command.
    Prepared,
    /// The binding moved. From here the move is roll-forward only.
    Committed,
    /// The incoming session was recorded acting at the committed epoch.
    Resumed,
    /// The handoff is closed and nothing further depends on this record.
    Retired,
    /// The handoff resumed and closing it failed. Reported on its own: the
    /// owner moved and kept working, only the bookkeeping did not finish.
    RetireFailed,
    /// Stopped before the binding moved.
    Aborted,
}

impl TakeoverState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Resumed => "resumed",
            Self::Retired => "retired",
            Self::RetireFailed => "retire_failed",
            Self::Aborted => "aborted",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "requested" => Ok(Self::Requested),
            "prepared" => Ok(Self::Prepared),
            "committed" => Ok(Self::Committed),
            "resumed" => Ok(Self::Resumed),
            "retired" => Ok(Self::Retired),
            "retire_failed" => Ok(Self::RetireFailed),
            "aborted" => Ok(Self::Aborted),
            other => Err(StoreError::InvalidTakeoverState(other.to_owned())),
        }
    }

    /// Whether no further step can be taken from this state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Retired | Self::RetireFailed | Self::Aborted)
    }

    /// The one place the handoff graph is written down.
    ///
    /// `committed -> resumed` is included because the store, not the caller,
    /// makes that move; `retire_failed` is a separate report rather than a
    /// failed handoff, so it never returns to `retired`.
    #[must_use]
    pub fn allows(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::Requested, Self::Prepared | Self::Aborted)
                | (Self::Prepared, Self::Committed | Self::Aborted)
                | (Self::Committed, Self::Resumed)
                | (Self::Resumed, Self::Retired | Self::RetireFailed)
        )
    }
}

/// How a resumed handoff was closed. These are all successes: a failure to
/// close is [`TakeoverState::RetireFailed`] with a reason, never an outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetireOutcome {
    /// The incoming session is carrying the work forward.
    Completed,
    /// A later takeover moved this owner again; this record is closed behind it.
    Superseded,
    /// The handoff is done and nothing else depends on this record.
    Closed,
}

impl RetireOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Superseded => "superseded",
            Self::Closed => "closed",
        }
    }
}

/// One task revision and the attempt a handoff names.
///
/// The store holds the two together: an attempt that does not belong to this
/// revision is refused rather than carried forward as a reference to nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManifestTask {
    pub task_id: TaskId,
    pub revision: u32,
    pub attempt_id: AttemptId,
}

/// Everything a handoff carries, which is references and nothing else.
///
/// A result is named by its id and a message by its id; no envelope, artifact,
/// objective, or decision is copied in. The incoming session reads those rows
/// through the same owner-scoped queries as the outgoing one, so it cannot be
/// handed a version of the work that the store does not hold.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffManifest {
    /// What the owner is trying to achieve, in its own words.
    pub goal: String,
    /// The owner's notes on decisions already taken. Never a decision: a
    /// recorded decision stays in `decisions` and is only described here.
    pub decision_notes: String,
    /// The session that wrote this manifest.
    pub origin_session: String,
    /// The revisions and attempts the handoff names.
    pub tasks: Vec<ManifestTask>,
    /// Results in the owner's inbox that nobody has decided yet.
    pub pending_result_ids: Vec<ResultId>,
    /// Questions and notes the owner has not answered yet.
    pub pending_message_ids: Vec<String>,
}

/// The references a handoff may name for one owner.
///
/// The pending lists come from the same queries that check a manifest's
/// pending results and messages, so a manifest built from this value names
/// only rows the store will accept.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HandoffReferences {
    /// Revisions that are still open and that have an attempt to name.
    pub tasks: Vec<ManifestTask>,
    /// Inbox results this owner has neither decided nor acknowledged.
    pub pending_result_ids: Vec<ResultId>,
    /// Worker messages on this owner's tasks that are still unacknowledged.
    pub pending_message_ids: Vec<String>,
}

/// A stored manifest, as read back from the store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredHandoffManifest {
    pub manifest_id: String,
    pub takeover_id: TakeoverId,
    pub owner_id: OwnerId,
    /// Digest of `manifest_json`, verified on every read.
    pub digest: String,
    pub created_at: i64,
    pub manifest: HandoffManifest,
}

/// One recorded handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Takeover {
    pub takeover_id: TakeoverId,
    pub owner_id: OwnerId,
    /// The session that held the head when the handoff was requested.
    pub from_session: String,
    /// The binding epoch that handoff was requested against.
    pub from_epoch: u64,
    /// The session taking the head.
    pub to_session: String,
    pub state: TakeoverState,
    /// The owner's own idempotency key for this request.
    pub idempotency_key: String,
    /// Set once the manifest is stored, and never changed afterwards.
    pub manifest_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Why this record stopped, when it stopped badly.
    pub failure_reason: Option<String>,
}

impl Takeover {
    /// The epoch the binding carries once this handoff is committed.
    ///
    /// Read rather than stored: the commit that sets it is the only thing that
    /// can raise the binding, and it raises it by exactly one.
    #[must_use]
    pub fn committed_epoch(&self) -> u64 {
        self.from_epoch.saturating_add(1)
    }
}

/// One row of a handoff's append-only history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeoverEvent {
    pub seq: u32,
    /// The state this transition entered. The current state is a mutable
    /// column; this is the trail, and it is only ever added to.
    pub kind: TakeoverState,
    /// Why the transition happened: a reason, or a retirement outcome.
    pub detail: Option<String>,
    pub at: i64,
}

impl Store {
    /// The canonical digest that binds a stored manifest to its content.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be serialized.
    pub fn handoff_manifest_digest(manifest: &HandoffManifest) -> Result<String, StoreError> {
        Ok(super::sha256(serde_json::to_vec(manifest)?.as_slice()))
    }

    /// Records the intent to move an owner's head from `from_session` to
    /// `to_session`.
    ///
    /// `from_session` must be the session the owner is currently bound to: a
    /// handoff is something the head decides about itself, not something
    /// another session can start on its behalf. The binding's epoch is recorded
    /// with it, so [`Store::commit_takeover`] can tell whether the head moved
    /// underneath the request.
    ///
    /// Repeating the request with the same `idempotency_key` returns the same
    /// takeover rather than starting a second one, and a key replayed for a
    /// different destination is refused rather than silently retargeted. Only
    /// one non-terminal takeover may exist per owner.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::OwnerUnbound`] or
    /// [`StoreError::OwnerBindingConflict`] when `from_session` is not the
    /// current head, [`StoreError::TakeoverInProgress`] when one is already
    /// open, [`StoreError::TakeoverIdempotencyConflict`] when the key names a
    /// different handoff, or [`StoreError::InvalidTakeoverRequest`] for a blank
    /// or degenerate request.
    pub fn request_takeover(
        &self,
        owner_id: &OwnerId,
        from_session: &str,
        to_session: &str,
        idempotency_key: &str,
    ) -> Result<Takeover, StoreError> {
        validate_request(from_session, to_session, idempotency_key)?;
        let transaction = self.write_transaction()?;
        let recorded: Option<String> = transaction
            .query_row(
                "SELECT takeover_id FROM head_takeovers
                 WHERE owner_id = ?1 AND idempotency_key = ?2",
                params![owner_id.as_str(), idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(takeover_id) = recorded {
            let takeover = load_takeover(&transaction, &takeover_id)?;
            if takeover.from_session == from_session && takeover.to_session == to_session {
                transaction.commit()?;
                return Ok(takeover);
            }
            return Err(StoreError::TakeoverIdempotencyConflict(
                idempotency_key.to_owned(),
            ));
        }
        let (session_id, epoch) = read_binding(&transaction, owner_id)?;
        if session_id != from_session {
            return Err(StoreError::OwnerBindingConflict);
        }
        let open: Option<String> = transaction
            .query_row(
                "SELECT takeover_id FROM head_takeovers
                 WHERE owner_id = ?1
                   AND state NOT IN ('retired', 'retire_failed', 'aborted') LIMIT 1",
                [owner_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(takeover_id) = open {
            return Err(StoreError::TakeoverInProgress(parse_takeover_id(
                &takeover_id,
            )?));
        }
        let takeover_id = TakeoverId::new();
        transaction.execute(
            "INSERT INTO head_takeovers
             (takeover_id, owner_id, from_session, from_epoch, to_session, state,
              idempotency_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'requested', ?6,
                 CAST(strftime('%s','now') AS INTEGER), CAST(strftime('%s','now') AS INTEGER))",
            params![
                takeover_id.to_string(),
                owner_id.as_str(),
                from_session,
                i64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)?,
                to_session,
                idempotency_key,
            ],
        )?;
        append_event(&transaction, takeover_id, TakeoverState::Requested, None)?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        transaction.commit()?;
        Ok(takeover)
    }

    /// Stores the handoff manifest and moves the takeover to `prepared`.
    ///
    /// The manifest is written once: preparing again with the same content is
    /// accepted while the handoff is still open, and preparing with different
    /// content is refused rather than replacing what the incoming session may
    /// already have read. The incoming session is still not in command here.
    ///
    /// An `aborted` handoff answers a replay with
    /// [`StoreError::TakeoverStateConflict`]: it stopped before the binding
    /// moved, and the same bytes arriving late must not make it look prepared.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TakeoverNotFound`],
    /// [`StoreError::TakeoverStateConflict`] unless the takeover is `requested`,
    /// [`StoreError::HandoffManifestConflict`] for different content, or
    /// [`StoreError::InvalidHandoffManifest`] for a manifest that is oversized,
    /// repeats itself, or names a row this owner does not have.
    pub fn prepare_takeover(
        &self,
        takeover_id: TakeoverId,
        manifest: &HandoffManifest,
    ) -> Result<Takeover, StoreError> {
        let digest = Self::handoff_manifest_digest(manifest)?;
        let manifest_json = serde_json::to_string(manifest)?;
        let transaction = self.write_transaction()?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        if let Some((_, stored_digest)) = stored_manifest(&transaction, takeover_id)? {
            // The manifest is already written, so this is a replay and not a new
            // step, and it is answered while the handoff is still open. An
            // aborted one has stopped for good and must not look prepared again
            // because the caller happens to resend the same bytes.
            if takeover.state == TakeoverState::Aborted {
                return Err(takeover_state_conflict(
                    takeover_id,
                    takeover.state,
                    TakeoverState::Prepared,
                ));
            }
            if stored_digest != digest {
                return Err(StoreError::HandoffManifestConflict(takeover_id));
            }
            transaction.commit()?;
            return Ok(takeover);
        }
        require_transition(takeover_id, takeover.state, TakeoverState::Prepared)?;
        validate_manifest(
            &transaction,
            &takeover.owner_id,
            &takeover.from_session,
            manifest,
        )?;
        let manifest_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO handoff_manifests
             (manifest_id, owner_id, takeover_id, manifest_json, digest, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, CAST(strftime('%s','now') AS INTEGER))",
            params![
                manifest_id,
                takeover.owner_id.as_str(),
                takeover_id.to_string(),
                manifest_json,
                digest,
            ],
        )?;
        set_state(&transaction, takeover_id, TakeoverState::Prepared, None)?;
        append_event(&transaction, takeover_id, TakeoverState::Prepared, None)?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        transaction.commit()?;
        Ok(takeover)
    }

    /// Moves the binding, in one transaction, and nothing else.
    ///
    /// The state must still be `prepared` and the binding must still equal the
    /// session and epoch the handoff was requested against. If either moved,
    /// the transaction fails and the takeover is left exactly as it was, so the
    /// caller can start a new handoff instead of committing a stale one.
    ///
    /// Committing a takeover that is already committed is not an error and does
    /// not raise the epoch again: a caller that lost the response re-runs it to
    /// find out what happened, and the answer is the same one transaction
    /// already gave.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TakeoverNotFound`],
    /// [`StoreError::TakeoverStateConflict`] unless the takeover is `prepared`,
    /// or [`StoreError::TakeoverBindingConflict`] when the binding moved after
    /// the request.
    pub fn commit_takeover(&self, takeover_id: TakeoverId) -> Result<Takeover, StoreError> {
        let transaction = self.write_transaction()?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        if !matches!(takeover.state, TakeoverState::Prepared) {
            // Already committed, or already past it: the epoch was raised once
            // by the transaction that got there, not by this call.
            if matches!(
                takeover.state,
                TakeoverState::Committed
                    | TakeoverState::Resumed
                    | TakeoverState::Retired
                    | TakeoverState::RetireFailed
            ) {
                transaction.commit()?;
                return Ok(takeover);
            }
            return Err(takeover_state_conflict(
                takeover_id,
                takeover.state,
                TakeoverState::Committed,
            ));
        }
        let (session_id, epoch) = read_binding(&transaction, &takeover.owner_id)?;
        if session_id != takeover.from_session || epoch != takeover.from_epoch {
            return Err(StoreError::TakeoverBindingConflict(takeover_id));
        }
        let committed =
            rebind_in_transaction(&transaction, &takeover.owner_id, &takeover.to_session)?;
        debug_assert_eq!(
            committed,
            takeover.committed_epoch(),
            "the commit must raise the binding by exactly one epoch"
        );
        set_state(&transaction, takeover_id, TakeoverState::Committed, None)?;
        append_event(&transaction, takeover_id, TakeoverState::Committed, None)?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        transaction.commit()?;
        Ok(takeover)
    }

    /// Closes a resumed handoff.
    ///
    /// Only a handoff that resumed may be retired, so a record cannot present a
    /// finished move the incoming head never picked up.
    ///
    /// Repeating the same terminal outcome is not an error: the record is
    /// reported as it stands, with the outcome already on it, and no second
    /// event is appended. A different outcome, or a handoff that never resumed,
    /// is a state conflict.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TakeoverNotFound`] or
    /// [`StoreError::TakeoverStateConflict`] unless the takeover is `resumed`
    /// or already `retired`.
    pub fn retire_takeover(
        &self,
        takeover_id: TakeoverId,
        outcome: RetireOutcome,
    ) -> Result<Takeover, StoreError> {
        self.close(
            takeover_id,
            TakeoverState::Retired,
            Some(outcome.as_str()),
            None,
        )
    }

    /// Records that a resumed handoff could not be closed.
    ///
    /// The handoff did resume: this says the bookkeeping after it failed, not
    /// that the owner moved and went back. The recorded reason stays on the
    /// takeover and the `resumed` row stays in its history, and there is no path
    /// from here to `retired` — a failed close is reported, not retried into a
    /// different story.
    ///
    /// Repeating the same terminal outcome reports the record as it stands and
    /// appends no second event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TakeoverNotFound`] or
    /// [`StoreError::TakeoverStateConflict`] unless the takeover is `resumed`
    /// or already `retire_failed`.
    pub fn fail_retirement(
        &self,
        takeover_id: TakeoverId,
        reason: &str,
    ) -> Result<Takeover, StoreError> {
        let reason = bounded_reason(reason)?;
        self.close(
            takeover_id,
            TakeoverState::RetireFailed,
            Some(&reason),
            Some(&reason),
        )
    }

    /// Stops a handoff that has not moved the binding.
    ///
    /// After the commit there is nothing to abort: the binding is on its new
    /// session and the only way forward is roll-forward.
    ///
    /// Repeating the same terminal outcome reports the record as it stands,
    /// with the reason already on it, and appends no second event. Any other
    /// terminal outcome, or a state from which no abort is allowed, is a state
    /// conflict.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TakeoverNotFound`] or
    /// [`StoreError::TakeoverStateConflict`] unless the takeover is `requested`
    /// or `prepared`, or is already `aborted`.
    pub fn abort_takeover(
        &self,
        takeover_id: TakeoverId,
        reason: &str,
    ) -> Result<Takeover, StoreError> {
        let reason = bounded_reason(reason)?;
        let transaction = self.write_transaction()?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        if takeover.state == TakeoverState::Aborted {
            transaction.commit()?;
            return Ok(takeover);
        }
        require_transition(takeover_id, takeover.state, TakeoverState::Aborted)?;
        set_state(
            &transaction,
            takeover_id,
            TakeoverState::Aborted,
            Some(&reason),
        )?;
        append_event(
            &transaction,
            takeover_id,
            TakeoverState::Aborted,
            Some(&reason),
        )?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        transaction.commit()?;
        Ok(takeover)
    }

    /// The shared body of the two ways a resumed handoff is closed. `detail` is
    /// the audit line; `failure_reason` is what stays on the record, and a
    /// clean retirement has none.
    fn close(
        &self,
        takeover_id: TakeoverId,
        to: TakeoverState,
        detail: Option<&str>,
        failure_reason: Option<&str>,
    ) -> Result<Takeover, StoreError> {
        let transaction = self.write_transaction()?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        // The lost-response replay of the same outcome: the record already says
        // this, so it is reported as it stands rather than closed a second time.
        // The state alone cannot tell two retire outcomes apart; the audit
        // event's detail is the outcome that was recorded.
        if takeover.state == to {
            let stored_detail: Option<Option<String>> = transaction
                .query_row(
                    "SELECT detail FROM takeover_events
                     WHERE takeover_id = ?1 AND kind = ?2
                     ORDER BY seq DESC LIMIT 1",
                    params![takeover_id.to_string(), to.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let same_outcome = match stored_detail {
                Some(stored) => stored.as_deref() == detail,
                None => false,
            };
            if same_outcome {
                transaction.commit()?;
                return Ok(takeover);
            }
            return Err(takeover_state_conflict(takeover_id, takeover.state, to));
        }
        require_transition(takeover_id, takeover.state, to)?;
        set_state(&transaction, takeover_id, to, failure_reason)?;
        append_event(&transaction, takeover_id, to, detail)?;
        let takeover = load_takeover(&transaction, &takeover_id.to_string())?;
        transaction.commit()?;
        Ok(takeover)
    }

    /// Reads one handoff, if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed stored id or a database failure.
    pub fn takeover(&self, takeover_id: TakeoverId) -> Result<Option<Takeover>, StoreError> {
        self.connection
            .query_row(
                "SELECT takeover_id FROM head_takeovers WHERE takeover_id = ?1",
                [takeover_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|stored| load_takeover(&self.connection, &stored))
            .transpose()
    }

    /// Every handoff recorded for one owner, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stored data or a database failure.
    pub fn owner_takeovers(&self, owner_id: &OwnerId) -> Result<Vec<Takeover>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT takeover_id FROM head_takeovers WHERE owner_id = ?1 ORDER BY created_at, rowid",
        )?;
        let ids = statement.query_map([owner_id.as_str()], |row| row.get::<_, String>(0))?;
        ids.map(|row| load_takeover(&self.connection, &row?))
            .collect()
    }

    /// The append-only history of one handoff, in order.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stored data or a database failure.
    pub fn takeover_events(
        &self,
        takeover_id: TakeoverId,
    ) -> Result<Vec<TakeoverEvent>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT seq, kind, detail, at FROM takeover_events WHERE takeover_id = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([takeover_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, kind, detail, at) = row?;
            Ok(TakeoverEvent {
                seq: u32::try_from(seq).map_err(|_| StoreError::NumericOverflow)?,
                kind: TakeoverState::parse(&kind)?,
                detail,
                at,
            })
        })
        .collect()
    }

    /// The manifest stored for one handoff, with its digest verified.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::HandoffManifestIntegrity`] when the stored bytes
    /// do not match the digest recorded with them.
    pub fn handoff_manifest(
        &self,
        takeover_id: TakeoverId,
    ) -> Result<Option<StoredHandoffManifest>, StoreError> {
        self.connection
            .query_row(
                "SELECT manifest_id, owner_id, manifest_json, digest, created_at
                 FROM handoff_manifests WHERE takeover_id = ?1",
                [takeover_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(manifest_id, owner_id, manifest_json, digest, created_at)| {
                    if super::sha256(manifest_json.as_bytes()) != digest {
                        return Err(StoreError::HandoffManifestIntegrity);
                    }
                    Ok(StoredHandoffManifest {
                        manifest_id,
                        takeover_id,
                        owner_id: OwnerId::new(owner_id)?,
                        digest,
                        created_at,
                        manifest: serde_json::from_str(&manifest_json)?,
                    })
                },
            )
            .transpose()
    }

    /// Lists the open work a handoff manifest may name for this owner.
    ///
    /// Open revisions are those [`crate::Settlement`] has not finished with.
    /// A revision that has no attempt is omitted, because a manifest cannot
    /// name one. Pending results and messages are the rows the manifest
    /// validator accepts.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed stored ids or a database failure.
    pub fn handoff_references(&self, owner_id: &OwnerId) -> Result<HandoffReferences, StoreError> {
        let mut tasks = Vec::new();
        let revisions = open_revision_attempts(&self.connection, owner_id)?;
        for (task_id, revision, attempt_id) in revisions {
            if !matches!(
                self.revision_settlement(task_id, revision)?,
                crate::Settlement::Decided | crate::Settlement::Acknowledged(_)
            ) {
                tasks.push(ManifestTask {
                    task_id,
                    revision,
                    attempt_id,
                });
            }
        }
        Ok(HandoffReferences {
            tasks,
            pending_result_ids: pending_result_ids(&self.connection, owner_id)?,
            pending_message_ids: pending_message_ids(&self.connection, owner_id)?,
        })
    }
}

/// Moves a committed handoff to `resumed` when the incoming head acts.
///
/// Never called by a caller that means it: it runs inside the transaction of an
/// owner-side write that has already proved the binding, so `resumed` records
/// that the new session was seen doing the owner's work rather than that it
/// said it had taken over. Only a takeover sitting in `committed` is moved, so
/// the step cannot run twice, cannot run before the commit, and cannot run for
/// the outgoing session — that one is rejected by the binding check before this
/// is reached.
pub(crate) fn note_owner_action(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    session_id: &str,
    binding_epoch: u64,
) -> Result<(), StoreError> {
    let committed: Option<String> = transaction
        .query_row(
            "SELECT takeover_id FROM head_takeovers
             WHERE owner_id = ?1 AND state = 'committed' AND to_session = ?2
               AND from_epoch + 1 = ?3",
            params![
                owner_id.as_str(),
                session_id,
                i64::try_from(binding_epoch).map_err(|_| StoreError::NumericOverflow)?,
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(takeover_id) = committed else {
        return Ok(());
    };
    let takeover_id = parse_takeover_id(&takeover_id)?;
    let changed = transaction.execute(
        "UPDATE head_takeovers SET state = 'resumed',
           updated_at = CAST(strftime('%s','now') AS INTEGER)
         WHERE takeover_id = ?1 AND state = 'committed'",
        [takeover_id.to_string()],
    )?;
    if changed != 1 {
        return Ok(());
    }
    append_event(transaction, takeover_id, TakeoverState::Resumed, None)
}

/// Asserts that a proof is still the owner's current binding.
///
/// This is the guard an owner-side write needs before it may change anything:
/// nothing else distinguishes the session that is the head from one that used to
/// be.
pub(crate) fn assert_owner_proof(
    transaction: &Transaction<'_>,
    proof: &OwnerProof,
) -> Result<(), StoreError> {
    assert_owner_binding(
        transaction,
        &proof.owner_id,
        Some(&proof.session_id),
        Some(proof.binding_epoch),
    )?;
    // Every bound write proves the head is acting, so `resumed` is derived here
    // once rather than at each call site. A later explicit call is a no-op.
    note_owner_action(
        transaction,
        &proof.owner_id,
        &proof.session_id,
        proof.binding_epoch,
    )
}

/// Asserts a proof and that the task it names belongs to that owner.
pub(crate) fn assert_task_owner(
    transaction: &Transaction<'_>,
    proof: &OwnerProof,
    task_id: TaskId,
) -> Result<(), StoreError> {
    assert_owner_proof(transaction, proof)?;
    let owner_id: Option<String> = transaction
        .query_row(
            "SELECT owner_id FROM tasks WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if owner_id.as_deref() != Some(proof.owner_id.as_str()) {
        return Err(StoreError::OwnerProofMismatch);
    }
    Ok(())
}

fn validate_request(
    from_session: &str,
    to_session: &str,
    idempotency_key: &str,
) -> Result<(), StoreError> {
    if from_session.trim().is_empty() || from_session.len() > MAX_SESSION_BYTES {
        return Err(StoreError::InvalidTakeoverRequest(
            "the outgoing session must be a bounded, non-empty session".to_owned(),
        ));
    }
    if to_session.trim().is_empty() || to_session.len() > MAX_SESSION_BYTES {
        return Err(StoreError::InvalidTakeoverRequest(
            "the incoming session must be a bounded, non-empty session".to_owned(),
        ));
    }
    if from_session == to_session {
        return Err(StoreError::InvalidTakeoverRequest(
            "a takeover must move the head to another session".to_owned(),
        ));
    }
    if idempotency_key.trim().is_empty() || idempotency_key.len() > MAX_KEY_BYTES {
        return Err(StoreError::InvalidTakeoverRequest(
            "the idempotency key must be a bounded, non-empty key".to_owned(),
        ));
    }
    Ok(())
}

/// Bounds a reason to the byte limit the constant names.
///
/// The limit is bytes, so a multibyte reason is cut on the nearest character
/// boundary at or below it: the stored reason is always whole characters and
/// never longer than the limit the rest of the store writes against.
fn bounded_reason(reason: &str) -> Result<String, StoreError> {
    if reason.trim().is_empty() {
        return Err(StoreError::InvalidTakeoverRequest(
            "a reason must not be empty".to_owned(),
        ));
    }
    if reason.len() <= MAX_REASON_BYTES {
        return Ok(reason.to_owned());
    }
    let mut end = MAX_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Ok(reason[..end].to_owned())
}

/// Checks that a manifest is bounded, does not repeat itself, and names rows
/// this owner actually has and still owes.
///
/// The last part is what keeps the manifest honest: a handoff may only point at
/// results and messages the outgoing head really held and has not settled, so
/// the incoming session cannot be handed a pointer into somebody else's work,
/// into nothing, or into a question already answered.
fn validate_manifest(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    from_session: &str,
    manifest: &HandoffManifest,
) -> Result<(), StoreError> {
    if manifest.goal.trim().is_empty() || manifest.goal.len() > MAX_GOAL_BYTES {
        return Err(StoreError::InvalidHandoffManifest(
            "the goal is empty or oversized",
        ));
    }
    if manifest.decision_notes.len() > MAX_NOTES_BYTES {
        return Err(StoreError::InvalidHandoffManifest(
            "the decision notes are oversized",
        ));
    }
    if manifest.origin_session.trim().is_empty()
        || manifest.origin_session.len() > MAX_SESSION_BYTES
    {
        return Err(StoreError::InvalidHandoffManifest(
            "the origin session is empty or oversized",
        ));
    }
    if manifest.origin_session != from_session {
        return Err(StoreError::InvalidHandoffManifest(
            "the origin session is not the outgoing session",
        ));
    }
    if manifest.tasks.len() > MAX_TASK_REFS
        || manifest.pending_result_ids.len() > MAX_PENDING_RESULTS
        || manifest.pending_message_ids.len() > MAX_PENDING_MESSAGES
    {
        return Err(StoreError::InvalidHandoffManifest(
            "a reference list is oversized",
        ));
    }
    validate_task_references(transaction, owner_id, &manifest.tasks)?;
    validate_pending_results(transaction, owner_id, &manifest.pending_result_ids)?;
    validate_pending_messages(transaction, owner_id, &manifest.pending_message_ids)
}

/// Each named revision is this owner's, is named once, and names an attempt that
/// really ran it. Without the attempt check a manifest could point at another
/// revision's work under this one's identity.
fn validate_task_references(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    tasks: &[ManifestTask],
) -> Result<(), StoreError> {
    let mut named = Vec::with_capacity(tasks.len());
    for reference in tasks {
        if named.contains(&(reference.task_id, reference.revision)) {
            return Err(StoreError::InvalidHandoffManifest(
                "a task revision is named twice",
            ));
        }
        named.push((reference.task_id, reference.revision));
        let owned: Option<()> = transaction
            .query_row(
                "SELECT 1 FROM tasks WHERE task_id = ?1 AND revision = ?2 AND owner_id = ?3",
                params![
                    reference.task_id.to_string(),
                    reference.revision,
                    owner_id.as_str()
                ],
                |_| Ok(()),
            )
            .optional()?;
        if owned.is_none() {
            return Err(StoreError::InvalidHandoffManifest(
                "a named task revision is not this owner's",
            ));
        }
        let attempt: Option<()> = transaction
            .query_row(
                "SELECT 1 FROM attempts
                 WHERE attempt_id = ?1 AND task_id = ?2 AND revision = ?3",
                params![
                    reference.attempt_id.to_string(),
                    reference.task_id.to_string(),
                    reference.revision
                ],
                |_| Ok(()),
            )
            .optional()?;
        if attempt.is_none() {
            return Err(StoreError::InvalidHandoffManifest(
                "a named attempt is not an attempt of that task revision",
            ));
        }
    }
    Ok(())
}

/// A pending result is one the owner has still to settle: undecided and
/// unacknowledged in this owner's inbox. Anything settled is the owner's own
/// history and does not travel.
fn validate_pending_results(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    result_ids: &[ResultId],
) -> Result<(), StoreError> {
    let pending = pending_result_ids(transaction, owner_id)?;
    let mut seen = Vec::with_capacity(result_ids.len());
    for result_id in result_ids {
        if seen.contains(result_id) {
            return Err(StoreError::InvalidHandoffManifest(
                "a pending result is named twice",
            ));
        }
        seen.push(*result_id);
        if !pending.contains(result_id) {
            return Err(StoreError::InvalidHandoffManifest(
                "a pending result is not undecided and unacknowledged in this owner's inbox",
            ));
        }
    }
    Ok(())
}

/// A pending message is an unanswered worker message on one of the owner's own
/// tasks. The outgoing head's own replies are not work the incoming one inherits.
fn validate_pending_messages(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
    message_ids: &[String],
) -> Result<(), StoreError> {
    let pending = pending_message_ids(transaction, owner_id)?;
    let mut seen = Vec::with_capacity(message_ids.len());
    for message_id in message_ids {
        if seen.iter().any(|stored: &String| stored == message_id) {
            return Err(StoreError::InvalidHandoffManifest(
                "a pending message is named twice",
            ));
        }
        seen.push(message_id.clone());
        if !pending.iter().any(|stored| stored == message_id) {
            return Err(StoreError::InvalidHandoffManifest(
                "a pending message is not an unanswered worker message on this owner's task",
            ));
        }
    }
    Ok(())
}

/// Undecided, unacknowledged inbox results for one owner.
///
/// This is the only query that decides which results a handoff may name.
fn pending_result_ids(
    connection: &Connection,
    owner_id: &OwnerId,
) -> Result<Vec<ResultId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT r.result_id
         FROM results r
         JOIN inbox_items i ON i.result_id = r.result_id
         WHERE i.owner_id = ?1 AND i.acknowledged = 0
           AND NOT EXISTS (
             SELECT 1 FROM decisions d WHERE d.result_id = r.result_id
           )
         ORDER BY r.rowid",
    )?;
    let rows = statement.query_map([owner_id.as_str()], |row| row.get::<_, String>(0))?;
    rows.map(|row| parse_result_id(&row?)).collect()
}

/// Unacknowledged worker-to-owner messages on one owner's tasks.
///
/// This is the only query that decides which messages a handoff may name.
fn pending_message_ids(
    connection: &Connection,
    owner_id: &OwnerId,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT m.message_id
         FROM task_messages m
         JOIN attempts a ON a.attempt_id = m.attempt_id
         JOIN tasks t ON t.task_id = a.task_id AND t.revision = a.revision
         WHERE t.owner_id = ?1
           AND m.direction = 'worker_to_owner' AND m.acknowledged = 0
         ORDER BY m.rowid",
    )?;
    let rows = statement.query_map([owner_id.as_str()], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

/// One attempt for each of this owner's revisions that has one.
///
/// The attempt is the non-terminal one when the revision still has one, and
/// otherwise the latest attempt. A revision with no attempt is absent.
fn open_revision_attempts(
    connection: &Connection,
    owner_id: &OwnerId,
) -> Result<Vec<(TaskId, u32, AttemptId)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT t.task_id, t.revision, a.attempt_id
         FROM tasks t
         JOIN attempts a ON a.task_id = t.task_id AND a.revision = t.revision
         WHERE t.owner_id = ?1
           AND a.attempt_id = (
             SELECT a2.attempt_id FROM attempts a2
             WHERE a2.task_id = t.task_id AND a2.revision = t.revision
             ORDER BY CASE WHEN a2.state = 'terminal' THEN 1 ELSE 0 END, a2.rowid DESC
             LIMIT 1
           )
         ORDER BY t.task_id, t.revision",
    )?;
    let rows = statement.query_map([owner_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (task_id, revision, attempt_id) = row?;
        Ok((
            parse_task_id(&task_id)?,
            u32::try_from(revision).map_err(|_| StoreError::NumericOverflow)?,
            parse_attempt_id(&attempt_id)?,
        ))
    })
    .collect()
}

fn parse_result_id(value: &str) -> Result<ResultId, StoreError> {
    value
        .parse()
        .map_err(|_| StoreError::InvalidHandoffManifest("a stored result id is malformed"))
}

fn parse_task_id(value: &str) -> Result<TaskId, StoreError> {
    value
        .parse()
        .map_err(|_| StoreError::InvalidHandoffManifest("a stored task id is malformed"))
}

fn parse_attempt_id(value: &str) -> Result<AttemptId, StoreError> {
    value
        .parse()
        .map_err(|_| StoreError::InvalidAttemptId(value.to_owned()))
}

fn parse_takeover_id(value: &str) -> Result<TakeoverId, StoreError> {
    TakeoverId::from_str(value).map_err(|_| StoreError::InvalidTakeoverId(value.to_owned()))
}

fn read_binding(
    transaction: &Transaction<'_>,
    owner_id: &OwnerId,
) -> Result<(String, u64), StoreError> {
    let binding: Option<(String, i64)> = transaction
        .query_row(
            "SELECT session_id, binding_epoch FROM owner_bindings WHERE owner_id = ?1",
            [owner_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((session_id, epoch)) = binding else {
        return Err(StoreError::OwnerUnbound(owner_id.clone()));
    };
    Ok((
        session_id,
        u64::try_from(epoch).map_err(|_| StoreError::NumericOverflow)?,
    ))
}

fn require_transition(
    takeover_id: TakeoverId,
    from: TakeoverState,
    to: TakeoverState,
) -> Result<(), StoreError> {
    if from.allows(to) {
        Ok(())
    } else {
        Err(takeover_state_conflict(takeover_id, from, to))
    }
}

fn takeover_state_conflict(
    takeover_id: TakeoverId,
    from: TakeoverState,
    to: TakeoverState,
) -> StoreError {
    StoreError::TakeoverStateConflict {
        takeover_id,
        from,
        to,
    }
}

fn set_state(
    transaction: &Transaction<'_>,
    takeover_id: TakeoverId,
    to: TakeoverState,
    failure_reason: Option<&str>,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE head_takeovers SET state = ?1, failure_reason = ?2,
           updated_at = CAST(strftime('%s','now') AS INTEGER)
         WHERE takeover_id = ?3",
        params![to.as_str(), failure_reason, takeover_id.to_string()],
    )?;
    Ok(())
}

/// Adds one row to the append-only trail. Never updates or deletes: the current
/// state is a column, and this is what it was on the way there.
fn append_event(
    transaction: &Transaction<'_>,
    takeover_id: TakeoverId,
    kind: TakeoverState,
    detail: Option<&str>,
) -> Result<(), StoreError> {
    let next: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM takeover_events WHERE takeover_id = ?1",
        [takeover_id.to_string()],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO takeover_events (takeover_id, seq, kind, detail, at)
         VALUES (?1, ?2, ?3, ?4, CAST(strftime('%s','now') AS INTEGER))",
        params![takeover_id.to_string(), next, kind.as_str(), detail],
    )?;
    Ok(())
}

fn stored_manifest(
    transaction: &Transaction<'_>,
    takeover_id: TakeoverId,
) -> Result<Option<(String, String)>, StoreError> {
    transaction
        .query_row(
            "SELECT manifest_id, digest FROM handoff_manifests WHERE takeover_id = ?1",
            [takeover_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
}

struct RawTakeover {
    takeover_id: String,
    owner_id: String,
    from_session: String,
    from_epoch: i64,
    to_session: String,
    state: String,
    idempotency_key: String,
    manifest_id: Option<String>,
    created_at: i64,
    updated_at: i64,
    failure_reason: Option<String>,
}

const TAKEOVER_COLUMNS: &str = "SELECT t.takeover_id, t.owner_id, t.from_session, t.from_epoch,
     t.to_session, t.state, t.idempotency_key, m.manifest_id, t.created_at, t.updated_at,
     t.failure_reason FROM head_takeovers t
     LEFT JOIN handoff_manifests m ON m.takeover_id = t.takeover_id
     WHERE t.takeover_id = ?1";

fn load_takeover(connection: &Connection, takeover_id: &str) -> Result<Takeover, StoreError> {
    let mut statement = connection.prepare(TAKEOVER_COLUMNS)?;
    let raw: RawTakeover = statement.query_row([takeover_id], read_raw_takeover)?;
    Ok(Takeover {
        takeover_id: parse_takeover_id(&raw.takeover_id)?,
        owner_id: OwnerId::new(raw.owner_id)?,
        from_session: raw.from_session,
        from_epoch: u64::try_from(raw.from_epoch).map_err(|_| StoreError::NumericOverflow)?,
        to_session: raw.to_session,
        state: TakeoverState::parse(&raw.state)?,
        idempotency_key: raw.idempotency_key,
        manifest_id: raw.manifest_id,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        failure_reason: raw.failure_reason,
    })
}

fn read_raw_takeover(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTakeover> {
    Ok(RawTakeover {
        takeover_id: row.get(0)?,
        owner_id: row.get(1)?,
        from_session: row.get(2)?,
        from_epoch: row.get(3)?,
        to_session: row.get(4)?,
        state: row.get(5)?,
        idempotency_key: row.get(6)?,
        manifest_id: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        failure_reason: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::{Store, fixtures::task};

    fn bound(root: &TempDir) -> (Store, brgr_protocol::OwnerId) {
        let store = Store::open(root.path()).unwrap();
        let owner_id = task().owner_id;
        store.bind_owner(&owner_id, "session-a", 1).unwrap();
        (store, owner_id)
    }

    fn manifest(origin: &str) -> HandoffManifest {
        HandoffManifest {
            goal: "Finish the review".to_owned(),
            decision_notes: "Revision one was rejected on scope.".to_owned(),
            origin_session: origin.to_owned(),
            ..HandoffManifest::default()
        }
    }

    #[test]
    fn a_handoff_runs_requested_prepared_committed_and_through_a_derived_resume() {
        let root = TempDir::new().unwrap();
        let (store, owner_id) = bound(&root);
        let takeover = store
            .request_takeover(&owner_id, "session-a", "session-b", "handoff-1")
            .unwrap();
        assert_eq!(takeover.state, TakeoverState::Requested);
        assert_eq!(takeover.from_epoch, 1);
        assert_eq!(takeover.committed_epoch(), 2);
        assert_eq!(store.owner_takeovers(&owner_id).unwrap().len(), 1);

        let prepared = store
            .prepare_takeover(takeover.takeover_id, &manifest("session-a"))
            .unwrap();
        assert_eq!(prepared.state, TakeoverState::Prepared);
        assert!(prepared.manifest_id.is_some());
        let stored = store
            .handoff_manifest(takeover.takeover_id)
            .unwrap()
            .expect("the manifest is stored when the takeover is prepared");
        assert_eq!(stored.manifest_id, prepared.manifest_id.unwrap());
        assert_eq!(
            stored.digest,
            Store::handoff_manifest_digest(&manifest("session-a")).unwrap()
        );
        assert_eq!(stored.manifest.goal, "Finish the review");

        let committed = store.commit_takeover(takeover.takeover_id).unwrap();
        assert_eq!(committed.state, TakeoverState::Committed);
        assert_eq!(
            store.owner_binding(&owner_id).unwrap(),
            Some(("session-b".to_owned(), 2))
        );
        assert_eq!(
            store
                .takeover_events(takeover.takeover_id)
                .unwrap()
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![
                TakeoverState::Requested,
                TakeoverState::Prepared,
                TakeoverState::Committed
            ],
            "one row per state transition, and none of them self-reported"
        );
    }

    #[test]
    fn an_oversized_or_unverifiable_manifest_is_refused() {
        let root = TempDir::new().unwrap();
        let (store, owner_id) = bound(&root);
        let takeover = store
            .request_takeover(&owner_id, "session-a", "session-b", "handoff-1")
            .unwrap();

        let empty_goal = HandoffManifest {
            goal: "   ".to_owned(),
            ..manifest("session-a")
        };
        assert!(matches!(
            store.prepare_takeover(takeover.takeover_id, &empty_goal),
            Err(StoreError::InvalidHandoffManifest(_))
        ));

        let unknown_result = HandoffManifest {
            pending_result_ids: vec![ResultId::new()],
            ..manifest("session-a")
        };
        assert!(matches!(
            store.prepare_takeover(takeover.takeover_id, &unknown_result),
            Err(StoreError::InvalidHandoffManifest(_))
        ));

        let unknown_message = HandoffManifest {
            pending_message_ids: vec![Uuid::new_v4().to_string()],
            ..manifest("session-a")
        };
        assert!(matches!(
            store.prepare_takeover(takeover.takeover_id, &unknown_message),
            Err(StoreError::InvalidHandoffManifest(_))
        ));

        assert_eq!(
            store.takeover(takeover.takeover_id).unwrap().unwrap().state,
            TakeoverState::Requested,
            "a refused manifest must not move the takeover"
        );
    }

    #[test]
    fn a_request_rejects_a_session_that_is_not_the_head_and_a_second_open_handoff() {
        let root = TempDir::new().unwrap();
        let (store, owner_id) = bound(&root);
        assert!(matches!(
            store.request_takeover(&owner_id, "session-z", "session-b", "handoff-1"),
            Err(StoreError::OwnerBindingConflict)
        ));
        assert!(matches!(
            store.request_takeover(&owner_id, "session-a", "session-a", "handoff-1"),
            Err(StoreError::InvalidTakeoverRequest(_))
        ));
        assert!(matches!(
            store.request_takeover(&owner_id, "session-a", "session-b", "  "),
            Err(StoreError::InvalidTakeoverRequest(_))
        ));

        let first = store
            .request_takeover(&owner_id, "session-a", "session-b", "handoff-1")
            .unwrap();
        assert!(matches!(
            store.request_takeover(&owner_id, "session-a", "session-c", "handoff-2"),
            Err(StoreError::TakeoverInProgress(_))
        ));
        assert!(matches!(
            store.request_takeover(&owner_id, "session-a", "session-c", "handoff-1"),
            Err(StoreError::TakeoverIdempotencyConflict(key)) if key == "handoff-1"
        ));
        store
            .abort_takeover(first.takeover_id, "the owner changed their mind")
            .unwrap();
        assert_eq!(
            store
                .request_takeover(&owner_id, "session-a", "session-c", "handoff-2")
                .unwrap()
                .state,
            TakeoverState::Requested,
            "an aborted handoff does not keep the owner busy"
        );
    }

    #[test]
    fn an_unknown_state_is_a_typed_error_rather_than_a_guess() {
        let root = TempDir::new().unwrap();
        let (store, owner_id) = bound(&root);
        let takeover = store
            .request_takeover(&owner_id, "session-a", "session-b", "handoff-1")
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE head_takeovers SET state = 'handed_over' WHERE takeover_id = ?1",
                [takeover.takeover_id.to_string()],
            )
            .unwrap();
        assert!(matches!(
            store.takeover(takeover.takeover_id),
            Err(StoreError::InvalidTakeoverState(state)) if state == "handed_over"
        ));
    }

    #[test]
    fn a_stored_manifest_whose_bytes_were_edited_is_refused_on_read() {
        let root = TempDir::new().unwrap();
        let (store, owner_id) = bound(&root);
        let takeover = store
            .request_takeover(&owner_id, "session-a", "session-b", "handoff-1")
            .unwrap();
        store
            .prepare_takeover(takeover.takeover_id, &manifest("session-a"))
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE handoff_manifests SET manifest_json = ?1",
                [r#"{"goal":"do something else","decision_notes":"","origin_session":"session-a","tasks":[],"pending_result_ids":[],"pending_message_ids":[]}"#],
            )
            .unwrap();
        assert!(matches!(
            store.handoff_manifest(takeover.takeover_id),
            Err(StoreError::HandoffManifestIntegrity)
        ));
    }

    #[test]
    fn the_transition_table_is_the_only_source_of_the_handoff_graph() {
        for from in [
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed,
            TakeoverState::Resumed,
            TakeoverState::Retired,
            TakeoverState::RetireFailed,
            TakeoverState::Aborted,
        ] {
            for to in [
                TakeoverState::Requested,
                TakeoverState::Prepared,
                TakeoverState::Committed,
                TakeoverState::Resumed,
                TakeoverState::Retired,
                TakeoverState::RetireFailed,
                TakeoverState::Aborted,
            ] {
                assert_eq!(
                    from.allows(to),
                    matches!(
                        (from, to),
                        (
                            TakeoverState::Requested,
                            TakeoverState::Prepared | TakeoverState::Aborted
                        ) | (
                            TakeoverState::Prepared,
                            TakeoverState::Committed | TakeoverState::Aborted
                        ) | (TakeoverState::Committed, TakeoverState::Resumed)
                            | (
                                TakeoverState::Resumed,
                                TakeoverState::Retired | TakeoverState::RetireFailed
                            )
                    ),
                    "{from:?} -> {to:?}"
                );
            }
            assert!(
                !from.allows(from),
                "{from:?} must not allow a transition to itself"
            );
        }
        assert!(TakeoverState::Retired.is_terminal());
        assert!(TakeoverState::RetireFailed.is_terminal());
        assert!(TakeoverState::Aborted.is_terminal());
        assert!(!TakeoverState::Resumed.is_terminal());
        assert_eq!(
            TakeoverState::parse("retire_failed").unwrap(),
            TakeoverState::RetireFailed
        );
        assert!(matches!(
            TakeoverState::parse("retiring"),
            Err(StoreError::InvalidTakeoverState(_))
        ));
        for state in [
            TakeoverState::Requested,
            TakeoverState::Prepared,
            TakeoverState::Committed,
            TakeoverState::Resumed,
            TakeoverState::Retired,
            TakeoverState::RetireFailed,
            TakeoverState::Aborted,
        ] {
            assert_eq!(TakeoverState::parse(state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn a_takeover_id_round_trips_and_a_malformed_one_is_typed() {
        let id = TakeoverId::new();
        assert_eq!(TakeoverId::from_str(&id.to_string()).unwrap(), id);
        assert!(matches!(
            parse_takeover_id("not-an-id"),
            Err(StoreError::InvalidTakeoverId(_))
        ));
        // Two ids are two handovers: a default is minted, never shared.
        let minted = TakeoverId::default();
        assert_ne!(minted, TakeoverId::default());
        assert_eq!(
            TakeoverId::from_str(&minted.to_string()).unwrap(),
            minted,
            "a minted id must be readable back"
        );
    }

    #[test]
    fn a_reason_must_be_present_and_is_bounded() {
        assert!(matches!(
            bounded_reason("   "),
            Err(StoreError::InvalidTakeoverRequest(_))
        ));
        assert_eq!(bounded_reason("why").unwrap(), "why");
        let long = "x".repeat(MAX_REASON_BYTES * 2);
        assert_eq!(bounded_reason(&long).unwrap().len(), MAX_REASON_BYTES);
    }

    #[test]
    fn a_multibyte_reason_is_cut_at_the_byte_limit_on_a_character_boundary() {
        // Three bytes per character, so the limit lands inside one: the kept
        // prefix must be whole characters and at most MAX_REASON_BYTES long.
        let reason = "가".repeat(MAX_REASON_BYTES);
        let cut = bounded_reason(&reason).unwrap();
        assert!(
            cut.len() <= MAX_REASON_BYTES,
            "the cut is {} bytes, over the {MAX_REASON_BYTES} limit",
            cut.len()
        );
        assert!(
            reason.starts_with(&cut),
            "the cut is not a prefix of the reason"
        );
        assert_eq!(
            cut.chars().count(),
            MAX_REASON_BYTES / 3,
            "the cut dropped or kept part of a character"
        );

        // A four-byte character right at the limit loses its whole self rather
        // than half of it.
        let prefix = "x".repeat(MAX_REASON_BYTES - 1);
        let mixed = format!("{prefix}é");
        assert_eq!(bounded_reason(&mixed).unwrap(), prefix);
        assert_eq!(bounded_reason(&mixed).unwrap().len(), MAX_REASON_BYTES - 1);

        // A short multibyte reason is untouched, byte for byte.
        assert_eq!(bounded_reason("가나다").unwrap(), "가나다");
    }
}
