//! `brgr head`: request, prepare, claim, show, and status for a takeover.
//!
//! The manifest is built from the store. The caller never names a task,
//! result, or message to carry across.

use std::{env, fs, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use brgr_protocol::{OwnerId, TaskId};
use brgr_store::{HandoffManifest, RetireOutcome, Store, Takeover, TakeoverId, TakeoverState};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Paths, pane_adapter::head_pane, print_value, require_owner, write_json_atomic};

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
    /// Incoming session takes an abandoned head (outgoing cannot run brgr).
    Claim {
        task: TaskId,
        /// Required: this is not a live cooperative handoff. Use `head request`
        /// when the current head can still type.
        #[arg(long)]
        abandoned: bool,
        /// What the owner is trying to finish. Required for the manifest.
        #[arg(long)]
        goal: String,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        key: Option<String>,
    },
    /// Open a new head in a Herdr pane and offer it this head's command.
    Handoff {
        task: TaskId,
        /// Herdr agent kind of the incoming head, for example claude or codex.
        #[arg(long)]
        kind: String,
        /// What the owner is trying to finish. Required for the manifest.
        #[arg(long)]
        goal: String,
        #[arg(long)]
        notes: Option<String>,
        /// Close this head's pane once the incoming head has resumed and retired.
        #[arg(long)]
        exit_old: bool,
        /// Extra argument for the incoming agent, for example a permission flag.
        #[arg(long = "agent-arg", allow_hyphen_values = true)]
        agent_args: Vec<String>,
    },
    /// Incoming head takes the command that `head handoff` offered.
    Take {
        task: TaskId,
        #[arg(long)]
        offer: String,
    },
    /// Retry closing the outgoing head's pane after a retired handoff.
    Exit { takeover: TakeoverId },
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
        } => prepare(&store, takeover, goal, notes, json_output)?,
        HeadCommand::Show { takeover } => show(&store, takeover, json_output)?,
        HeadCommand::Status { task } => status(&store, task, json_output)?,
        HeadCommand::Commit { takeover } => {
            commit(&store, takeover, json_output)?;
        }
        HeadCommand::Abort { takeover, reason } => {
            abort(&store, takeover, &reason, json_output)?;
        }
        HeadCommand::Retire { takeover, failed } => {
            retire(
                Some(paths),
                &store,
                takeover,
                failed.as_deref(),
                json_output,
            )?;
        }
        HeadCommand::Claim {
            task,
            abandoned,
            goal,
            notes,
            key,
        } => {
            claim(
                &store,
                task,
                abandoned,
                &goal,
                notes.as_deref(),
                key,
                json_output,
            )?;
        }
        HeadCommand::Handoff {
            task,
            kind,
            goal,
            notes,
            exit_old,
            agent_args,
        } => handoff(
            paths,
            &store,
            task,
            &HandoffArgs {
                kind,
                goal,
                notes,
                exit_old,
                agent_args,
            },
            json_output,
        )?,
        HeadCommand::Take { task, offer } => take(paths, &store, task, &offer, json_output)?,
        HeadCommand::Exit { takeover } => exit_outgoing(paths, &store, takeover, json_output)?,
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

fn prepare(
    store: &Store,
    takeover: TakeoverId,
    goal: String,
    notes: Option<String>,
    json_output: bool,
) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    let (session, _) = require_owner(store, &record.owner_id)?;
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
    Ok(())
}

fn show(store: &Store, takeover: TakeoverId, json_output: bool) -> Result<()> {
    let record = load_takeover(store, takeover)?;
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
    Ok(())
}

fn status(store: &Store, task: TaskId, json_output: bool) -> Result<()> {
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
                "from_epoch": takeover.from_epoch,
                "committed_epoch": match takeover.state {
                    TakeoverState::Requested
                    | TakeoverState::Prepared
                    | TakeoverState::Aborted => None,
                    TakeoverState::Committed
                    | TakeoverState::Resumed
                    | TakeoverState::Retired
                    | TakeoverState::RetireFailed => Some(takeover.committed_epoch()),
                },
                "updated_at": takeover.updated_at,
            })).collect::<Vec<_>>(),
        }),
        json_output,
    );
    Ok(())
}

fn claim(
    store: &Store,
    task: TaskId,
    abandoned: bool,
    goal: &str,
    notes: Option<&str>,
    key: Option<String>,
    json_output: bool,
) -> Result<()> {
    if !abandoned {
        bail!(
            "`brgr head claim` takes an abandoned head; pass --abandoned, or use `brgr head request` while the current head can still type"
        );
    }
    let owner_id = store.task(task)?.owner_id;
    let session = crate::current_session()?.context("a session is required")?;
    let key = key.unwrap_or_else(|| Uuid::new_v4().to_string());
    claim_core(
        store,
        &owner_id,
        &session,
        &Claim {
            goal,
            notes,
            key: &key,
            why: "`claim --abandoned`",
        },
        json_output,
    )?;
    Ok(())
}

struct Claim<'a> {
    goal: &'a str,
    notes: Option<&'a str>,
    key: &'a str,
    /// Names the path in the one stderr line a takeover without the outgoing
    /// head's own step leaves.
    why: &'a str,
}

/// Request, prepare and commit as the incoming session. `why` names the path in
/// the one stderr line a takeover without the outgoing head's step leaves.
fn claim_core(
    store: &Store,
    owner_id: &OwnerId,
    session: &str,
    claim: &Claim<'_>,
    json_output: bool,
) -> Result<Takeover> {
    let Claim {
        goal,
        notes,
        key,
        why,
    } = *claim;
    let (from_session, _) = store.owner_binding(owner_id)?.with_context(|| {
        format!("owner {owner_id} is unbound; run `brgr bind TASK --session {session}`")
    })?;
    if from_session == session {
        if let Some(existing) = store
            .owner_takeovers(owner_id)?
            .into_iter()
            .find(|takeover| takeover.idempotency_key == key && takeover.to_session == session)
        {
            if existing.state == TakeoverState::Aborted {
                bail!(
                    "takeover {} was aborted; claim again with a new --key",
                    existing.takeover_id
                );
            }
            let committed = store.commit_takeover(existing.takeover_id)?;
            let stored = store.handoff_manifest(committed.takeover_id)?;
            print_value(
                &json!({
                    "takeover": takeover_json(&committed),
                    "binding_epoch": committed.committed_epoch(),
                    "previous_session_stale": true,
                    "manifest": stored.map(|item| item.manifest),
                }),
                json_output,
            );
            return Ok(committed);
        }
        bail!("this session already holds the head; {why} is for a different session");
    }
    eprintln!(
        "brgr: taking the head of {owner_id} from {from_session} ({why}); owner-session check skipped"
    );
    let requested = store.request_takeover(owner_id, &from_session, session, key)?;
    if requested.state == TakeoverState::Aborted {
        bail!(
            "takeover {} was aborted; claim again with a new --key",
            requested.takeover_id
        );
    }
    if requested.state == TakeoverState::Requested {
        let references = store.handoff_references(owner_id)?;
        let manifest = HandoffManifest {
            goal: goal.to_owned(),
            decision_notes: notes.unwrap_or_default().to_owned(),
            origin_session: from_session.clone(),
            tasks: references.tasks,
            pending_result_ids: references.pending_result_ids,
            pending_message_ids: references.pending_message_ids,
        };
        store.prepare_takeover(requested.takeover_id, &manifest)?;
    }
    let committed = store.commit_takeover(requested.takeover_id)?;
    let stored = store.handoff_manifest(committed.takeover_id)?;
    print_value(
        &json!({
            "takeover": takeover_json(&committed),
            "binding_epoch": committed.committed_epoch(),
            "previous_session_stale": true,
            "manifest": stored.map(|item| item.manifest),
        }),
        json_output,
    );
    Ok(committed)
}

fn commit(store: &Store, takeover: TakeoverId, json_output: bool) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    owner_matches(&record.owner_id)?;
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
    owner_matches(&record.owner_id)?;
    let session = crate::current_session()?.context("a Codex session is required")?;
    if session != record.from_session && session != record.to_session {
        bail!("only the outgoing or incoming session may abort");
    }
    let aborted = store.abort_takeover(takeover, reason)?;
    print_value(&takeover_json(&aborted), json_output);
    Ok(())
}

fn retire(
    paths: Option<&Paths>,
    store: &Store,
    takeover: TakeoverId,
    failed: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    owner_matches(&record.owner_id)?;
    let session = crate::current_session()?.context("a Codex session is required")?;
    if session != record.to_session {
        bail!("only the incoming session may retire");
    }
    let retired = if let Some(reason) = failed {
        store.fail_retirement(takeover, reason)?
    } else {
        store.retire_takeover(takeover, RetireOutcome::Completed)?
    };
    let mut value = takeover_json(&retired);
    if failed.is_none() {
        let paths = paths.context("a home is required to close the outgoing head")?;
        if let Some(outgoing) = close_if_requested(paths, takeover) {
            value["outgoing_head"] = outgoing;
        }
    }
    print_value(&value, json_output);
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

/// What `head handoff` leaves for `take`: the permission to claim, and where the
/// outgoing head's pane is.
#[derive(Serialize, Deserialize)]
struct Offer {
    task: TaskId,
    owner_id: String,
    key: String,
    from_session: String,
    from_epoch: u64,
    from_pane: String,
    from_terminal: Option<String>,
    exit_old: bool,
    goal: String,
    notes: Option<String>,
}

/// Kept per takeover so `retire` and `exit` can find the outgoing pane.
#[derive(Serialize, Deserialize)]
struct Departure {
    from_pane: String,
    from_terminal: Option<String>,
    exit_old: bool,
    closed: bool,
}

struct HandoffArgs {
    kind: String,
    goal: String,
    notes: Option<String>,
    exit_old: bool,
    agent_args: Vec<String>,
}

fn handoffs_dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.home.join("handoffs");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn offer_path(paths: &Paths, key: &str) -> Result<PathBuf> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("offer key must be letters, digits and dashes");
    }
    Ok(handoffs_dir(paths)?.join(format!("offer-{key}.json")))
}

fn departure_path(paths: &Paths, takeover: TakeoverId) -> Result<PathBuf> {
    Ok(handoffs_dir(paths)?.join(format!("{takeover}.json")))
}

fn handoff(
    paths: &Paths,
    store: &Store,
    task: TaskId,
    args: &HandoffArgs,
    json_output: bool,
) -> Result<()> {
    let spec = store.task(task)?;
    let owner_id = spec.owner_id;
    let (session, epoch) = require_owner(store, &owner_id)?;
    let caller = crate::caller_pane::verified()
        .context("`head handoff` opens a pane beside this head, so it needs a verified Herdr pane; use `head request` outside Herdr")?;
    if store
        .owner_takeovers(&owner_id)?
        .iter()
        .any(|takeover| !takeover.state.is_terminal())
    {
        bail!("owner {owner_id} already has an open handoff; finish or abort it first");
    }
    let key = Uuid::new_v4().to_string();
    let offer = Offer {
        task,
        owner_id: owner_id.as_str().to_owned(),
        key: key.clone(),
        from_session: session,
        from_epoch: epoch,
        from_pane: caller.clone(),
        from_terminal: head_pane::terminal_of(&caller),
        exit_old: args.exit_old,
        goal: args.goal.clone(),
        notes: args.notes.clone(),
    };
    let path = offer_path(paths, &key)?;
    write_json_atomic(&path, &offer)?;
    // The incoming pane may not have this brgr on its PATH, and the control home
    // may have come from `--home`, which a child's environment does not carry.
    let brgr = std::env::var("BRGR_BIN")
        .ok()
        .filter(|path| !path.is_empty())
        .map_or_else(
            || {
                std::env::current_exe()
                    .map_or_else(|_| "brgr".to_owned(), |exe| exe.display().to_string())
            },
            |path| path,
        );
    let prompt = format!(
        "You are the incoming brgr head for owner {owner_id}. The outgoing head offered you its command. \
         Use this brgr for every command below: {brgr}. \
         1) Run `{brgr} head take {task} --offer {key}`; it prints the handoff manifest. \
         2) Read the open tasks with `{brgr} status {task} --tree`. \
         3) Goal to finish: {goal}. Resume it: decide pending results with `{brgr} result` (accept or reject), \
         answer worker questions, or take the next action on the open tasks. Do not restart workers. \
         4) Once you have done that, run `{brgr} head retire TAKEOVER` with the takeover id from step 1.",
        goal = args.goal,
    );
    let workspace = std::env::current_dir()?;
    let opened = head_pane::open_incoming(
        paths,
        &head_pane::IncomingSpec {
            caller: &caller,
            task,
            revision: spec.revision,
            workspace: &workspace,
            kind: &args.kind,
            agent_args: &args.agent_args,
            // The incoming head is its own session; it must not inherit this one's id.
            env: &[
                ("BRGR_SESSION_ID", String::new()),
                ("BRGR_OWNER_ID", owner_id.as_str().to_owned()),
                ("BRGR_HOME", paths.home.display().to_string()),
            ],
            label: "head",
            prompt: &prompt,
        },
    );
    let opened = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
    };
    print_value(
        &json!({
            "offer": key,
            "incoming_pane": opened.pane,
            "incoming_terminal": opened.terminal,
            "exit_old": args.exit_old,
            "next": "the incoming head runs `brgr head take` itself; this head stays the head until it does",
        }),
        json_output,
    );
    Ok(())
}

fn take(paths: &Paths, store: &Store, task: TaskId, key: &str, json_output: bool) -> Result<()> {
    let offer: Offer = serde_json::from_slice(
        &fs::read(offer_path(paths, key)?)
            .context("no such offer; the outgoing head makes one with `brgr head handoff`")?,
    )?;
    let owner_id = store.task(task)?.owner_id;
    if owner_id.as_str() != offer.owner_id {
        bail!("this offer is for owner {}, not {owner_id}", offer.owner_id);
    }
    let session = crate::current_session()?.context("a session is required")?;
    if let Some((bound, epoch)) = store.owner_binding(&owner_id)?
        && bound != session
        && (bound != offer.from_session || epoch != offer.from_epoch)
    {
        bail!("the head moved since this offer was made; ask the outgoing head for a new one");
    }
    let committed = claim_core(
        store,
        &owner_id,
        &session,
        &Claim {
            goal: &offer.goal,
            notes: offer.notes.as_deref(),
            key: &offer.key,
            why: "`head take`",
        },
        json_output,
    )?;
    let record = departure_path(paths, committed.takeover_id)?;
    if !record.exists() {
        write_json_atomic(
            &record,
            &Departure {
                from_pane: offer.from_pane.clone(),
                from_terminal: offer.from_terminal.clone(),
                exit_old: offer.exit_old,
                closed: false,
            },
        )?;
        let told = head_pane::tell_head(
            &offer.from_pane,
            &format!(
                "brgr: command of owner {owner_id} moved to another session. This session is stale for every owner command; do not run brgr owner commands. {}",
                if offer.exit_old {
                    "This pane closes after the new head has resumed and retired."
                } else {
                    "You may close this pane."
                }
            ),
        );
        if !told {
            eprintln!("brgr: the outgoing head's pane was not idle, so it was not told");
        }
    }
    Ok(())
}

/// Closes the outgoing pane when the handoff asked for it. Returns the account
/// to print, or `None` when nothing was asked.
fn close_if_requested(paths: &Paths, takeover: TakeoverId) -> Option<Value> {
    let record = departure_path(paths, takeover).ok()?;
    let mut departure: Departure = serde_json::from_slice(&fs::read(&record).ok()?).ok()?;
    if !departure.exit_old {
        return None;
    }
    if departure.closed {
        return Some(json!({ "closed": true, "pane": departure.from_pane }));
    }
    let keep = crate::caller_pane::verified().unwrap_or_default();
    match head_pane::close_outgoing(
        &departure.from_pane,
        departure.from_terminal.as_deref(),
        &keep,
    ) {
        Ok(()) => {
            departure.closed = true;
            let _ = write_json_atomic(&record, &departure);
            Some(json!({ "closed": true, "pane": departure.from_pane }))
        }
        Err(error) => {
            eprintln!(
                "brgr: the handoff is retired but the outgoing head was not closed: {error:#}; retry with `brgr head exit {takeover}`"
            );
            Some(json!({
                "closed": false,
                "pane": departure.from_pane,
                "reason": format!("{error:#}"),
            }))
        }
    }
}

fn exit_outgoing(
    paths: &Paths,
    store: &Store,
    takeover: TakeoverId,
    json_output: bool,
) -> Result<()> {
    let record = load_takeover(store, takeover)?;
    owner_matches(&record.owner_id)?;
    let session = crate::current_session()?.context("a session is required")?;
    if session != record.to_session {
        bail!("only the incoming session may close the outgoing head");
    }
    if record.state != TakeoverState::Retired {
        bail!(
            "the outgoing head is closed only after a clean retire; this handoff is {}",
            record.state.as_str()
        );
    }
    let outgoing = close_if_requested(paths, takeover)
        .context("this handoff did not ask for the outgoing head to be closed")?;
    print_value(&json!({ "outgoing_head": outgoing }), json_output);
    Ok(())
}
