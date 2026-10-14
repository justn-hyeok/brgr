//! `brgr head`: request, prepare, show, and status for a takeover.
//!
//! The manifest is built from the store. The caller never names a task,
//! result, or message to carry across.

use std::env;

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{OwnerId, TaskId};
use brgr_store::{HandoffManifest, RetireOutcome, Store, Takeover, TakeoverId};
use clap::Subcommand;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Paths, print_value, require_owner};

#[derive(Subcommand)]
pub(crate) enum HeadCommand {
    /// Record a handoff from the current head to another session.
    Request {
        task: TaskId,
        /// Session that will take the head.
        #[arg(long)]
        to: String,
        /// Idempotency key. A repeat with the same key returns the same record.
        #[arg(long)]
        key: Option<String>,
    },
    /// Store the handoff manifest built from the owner's open work.
    Prepare {
        takeover: TakeoverId,
        /// What the owner is trying to finish. Required, and bounded by the store.
        #[arg(long)]
        goal: String,
        /// Notes on decisions already taken. Not a decision.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Print one handoff, its events, and the digest-verified manifest.
    Show { takeover: TakeoverId },
    /// List this owner's handoffs, newest first.
    Status { task: TaskId },
    /// Transfer the owner binding to the incoming session.
    Commit { takeover: TakeoverId },
    /// Stop a takeover before the binding moves.
    Abort {
        takeover: TakeoverId,
        #[arg(long)]
        reason: String,
    },
    /// Close a resumed takeover, or record why closing failed.
    Retire {
        takeover: TakeoverId,
        #[arg(long)]
        failed: Option<String>,
    },
}

pub(crate) fn run(paths: &Paths, command: HeadCommand, json_output: bool) -> Result<()> {
    let store = Store::open(&paths.store)?;
    match command {
        HeadCommand::Request { task, to, key } => {
            request(&store, task, &to, key, json_output)?;
        }
        HeadCommand::Prepare {
            takeover,
            goal,
            notes,
        } => {
            let record = load_takeover(&store, takeover)?;
            let (session, _) = require_owner(&store, &record.owner_id)?;
            if record.from_session != session {
                bail!("only the outgoing session may prepare a takeover");
            }
            let references = store.handoff_references(&record.owner_id)?;
            let manifest = HandoffManifest {
                goal,
                decision_notes: notes.unwrap_or_default(),
                origin_session: session,
                tasks: references.tasks,
                pending_result_ids: references.pending_result_ids,
                pending_message_ids: references.pending_message_ids,
            };
            let prepared = store.prepare_takeover(takeover, &manifest)?;
            let mut value = takeover_json(&prepared);
            value["manifest"] = serde_json::to_value(&manifest)?;
            print_value(&value, json_output);
        }
        HeadCommand::Show { takeover } => {
            let record = load_takeover(&store, takeover)?;
            owner_matches(&record.owner_id)?;
            eprintln!(
                "brgr: observing a task owned by {}; read only",
                record.owner_id
            );
            let events = store.takeover_events(takeover)?;
            let manifest = store.handoff_manifest(takeover)?;
            print_value(
                &json!({
                    "takeover": takeover_json(&record),
                    "events": events.iter().map(|event| json!({
                        "seq": event.seq,
                        "kind": event.kind.as_str(),
                        "detail": event.detail,
                        "at": event.at,
                    })).collect::<Vec<_>>(),
                    "manifest": manifest.map(|stored| json!({
                        "manifest_id": stored.manifest_id,
                        "digest": stored.digest,
                        "created_at": stored.created_at,
                        "manifest": stored.manifest,
                    })),
                }),
                json_output,
            );
        }
        HeadCommand::Status { task } => {
            let owner_id = store.task(task)?.owner_id;
            owner_matches(&owner_id)?;
            let mut takeovers = store.owner_takeovers(&owner_id)?;
            takeovers.reverse();
            print_value(
                &json!({
                    "takeovers": takeovers.iter().map(|takeover| json!({
                        "takeover_id": takeover.takeover_id.to_string(),
                        "state": takeover.state.as_str(),
                        "from_session": takeover.from_session,
                        "to_session": takeover.to_session,
                        "epoch": takeover.from_epoch,
                        "updated_at": takeover.updated_at,
                    })).collect::<Vec<_>>(),
                }),
                json_output,
            );
        }
        HeadCommand::Commit { takeover } => {
            commit(&store, takeover, json_output)?;
        }
        HeadCommand::Abort { takeover, reason } => {
            abort(&store, takeover, &reason, json_output)?;
        }
        HeadCommand::Retire { takeover, failed } => {
            retire(&store, takeover, failed.as_deref(), json_output)?;
        }
    }
    Ok(())
}

fn request(
    store: &Store,
    task: TaskId,
    to: &str,
    key: Option<String>,
    json_output: bool,
) -> Result<()> {
    let owner_id = store.task(task)?.owner_id;
    let key = key.unwrap_or_else(|| Uuid::new_v4().to_string());
    if let Some(session) = crate::current_session()?
        && let Some(existing) = store
            .owner_takeovers(&owner_id)?
            .into_iter()
            .find(|takeover| {
                takeover.idempotency_key == key
                    && takeover.from_session == session
                    && takeover.to_session == to
            })
    {
        print_value(&takeover_json(&existing), json_output);
        return Ok(());
    }
    let (session, _) = require_owner(store, &owner_id)?;
    let takeover = store.request_takeover(&owner_id, &session, to, &key)?;
    print_value(&takeover_json(&takeover), json_output);
    Ok(())
}

fn commit(store: &Store, takeover: TakeoverId, json_output: bool) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    let session = crate::current_session()?.context("a Codex session is required")?;
    if record.to_session != session {
        bail!("only the incoming session may commit");
    }
    let committed = store.commit_takeover(takeover)?;
    print_value(
        &json!({
            "takeover": takeover_json(&committed),
            "binding_epoch": committed.committed_epoch(),
            "previous_session_stale": true,
        }),
        json_output,
    );
    Ok(())
}

fn abort(store: &Store, takeover: TakeoverId, reason: &str, json_output: bool) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    let session = crate::current_session()?.context("a Codex session is required")?;
    if session != record.from_session && session != record.to_session {
        bail!("only the outgoing or incoming session may abort");
    }
    let aborted = store.abort_takeover(takeover, reason)?;
    print_value(&takeover_json(&aborted), json_output);
    Ok(())
}

fn retire(
    store: &Store,
    takeover: TakeoverId,
    failed: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    let session = crate::current_session()?.context("a Codex session is required")?;
    if session != record.to_session {
        bail!("only the incoming session may retire");
    }
    let retired = if let Some(reason) = failed {
        store.fail_retirement(takeover, reason)?
    } else {
        store.retire_takeover(takeover, RetireOutcome::Completed)?
    };
    print_value(&takeover_json(&retired), json_output);
    Ok(())
}

fn load_takeover(store: &Store, takeover_id: TakeoverId) -> Result<Takeover> {
    store
        .takeover(takeover_id)?
        .with_context(|| format!("takeover {takeover_id} does not exist"))
}

/// The same owner mismatch `require_owner` reports, without demanding the head.
fn owner_matches(expected: &OwnerId) -> Result<()> {
    if let Ok(explicit_owner) = env::var("BRGR_OWNER_ID")
        && explicit_owner != expected.as_str()
    {
        bail!("task belongs to {expected}; BRGR_OWNER_ID differs");
    }
    Ok(())
}

fn takeover_json(takeover: &Takeover) -> Value {
    json!({
        "takeover_id": takeover.takeover_id.to_string(),
        "owner_id": takeover.owner_id.as_str(),
        "from_session": takeover.from_session,
        "from_epoch": takeover.from_epoch,
        "to_session": takeover.to_session,
        "state": takeover.state.as_str(),
        "idempotency_key": takeover.idempotency_key,
        "manifest_id": takeover.manifest_id,
        "created_at": takeover.created_at,
        "updated_at": takeover.updated_at,
        "failure_reason": takeover.failure_reason,
    })
}
