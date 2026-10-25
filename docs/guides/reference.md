# brgr reference

The detailed contract behind the [README](../../README.md): plugin details,
routes, pane mode, recursive workers, the completion loop, worktree cleanup,
and development.

## Herdr plugin details

Install the current public v2 plugin from GitHub:

```bash
herdr plugin install justn-hyeok/brgr --ref v2.10.6
```

The plugin builds `brgr` from the pinned Cargo lockfile, so a Rust toolchain is
required for installation. For local development, build with `cargo build
--release --locked -p brgr-cli` and run `herdr plugin link .` from this checkout.
The manifest is [herdr-plugin.toml](../../herdr-plugin.toml).

If the Herdr server's `PATH` does not contain `codex` (for example, with a
shell-managed Node installation), configure its absolute executable path from
your shell before opening the plugin Codex pane:

```bash
brgr config set-codex-executable "$(command -v codex)"
```

`brgr config clear-codex-executable` restores `PATH` lookup. When Herdr has not
yet reported the pane's Codex session ID, brgr verifies the exact Codex pane
and reports the bound session before delivering a completion notification.

Herdr exposes three workspace actions: **Open brgr status** opens a live,
read-only board of the latest 20 tasks; **Open Codex for brgr** starts a new
Codex pane in the selected worktree or workspace; **Check brgr** reports
integration and harness health. Opening the Codex pane installs or updates only
brgr-owned Codex hooks and its skill, then launches Codex with the plugin's
built binary on `PATH`. A private bridge tied to that pane executes brgr CLI
commands in the Herdr host, where supervisor process inspection is available;
Codex keeps its ordinary command sandbox and receives access only to an
ephemeral bridge directory. The host accepts managed task, result, decision,
health, and cleanup commands, pins them to the selected workspace and brgr
control home, and rejects harness mutation or integration commands. Plugin
installation itself does not run a paid model or activate a harness. Register
an approved executable with a small authorized scratch run outside the plugin
Codex pane before using it for a task. The board shows task status, route, and
decision state without revealing prompts or artifact contents.

See [the v2 plugin contract](v2-herdr-plugin.md) for ownership, context,
recovery, and verification boundaries.

Default execution uses the registered harness's native TUI in an owned Herdr
pane, with its full/YOLO recipe. Headless execution is only ever an explicit
per-call choice: `--headless`, `--mode headless`, or `--mode auto`. Auto chooses
headless only when no Herdr pane is available and reports the selected mode and
reason. After a pane is selected, launch failures and unsupported TUI options
fail without switching routes. brgr preserves the requested harness, model,
and effort; workers always run with full permissions, and `--permission` is
ignored. Unsupported TUI options fail before admission instead of selecting
headless. The separate `local.omp-herdr` route retains its legacy
`omp-role` presentation adapter. `--keep-pane` retains an owned pane.

| Route or operation | v1 status |
|---|---|
| OMP process, Cursor CLI, Command Code, Devin CLI, Codex CLI, GitHub Copilot CLI, Gemini CLI, Pi | Certified bounded fresh-run contract; exact model/effort only where the activated recipe supports it |
| GJC | Experimental: the recipe is kept and registered harnesses keep working, but it is not part of the certified set and is not rechecked on each release |
| Approved unfamiliar one-shot CLI | Supported after manifest contract test and authorized scratch activation |
| `local.omp-herdr` presentation | Optional best effort; external worker stop is not certified and uncertain failure is `lost` |
| Resume, in-flight steering, provider-side undo | Deferred; unsupported requests must fail rather than silently fall back |

`local.devin` runs `devin` in documented prompt-file print mode with smart
permissions and the non-interactive workspace-trust override. It uses the model
already selected in Devin CLI. Run `/model` once in an interactive Devin session
if that configured default is stale. Devin's current JSON catalog exceeds
brgr's bounded probe limit, so `brgr run OBJECTIVE --model MODEL` and `--effort` intentionally
fail for this route instead of guessing a family variant. See the
[v2.0.2 live receipt](../evidence/live-devin-cli-v2.0.2-2026-09-15.md).

`local.opencode` passes `--standalone` when `opencode run --help` documents it
(OpenCode 2), in its pane as well. Without it the task runs in OpenCode's shared
background service, which carries on after brgr stops the run. OpenCode checks
model names itself, so `--model provider/model#variant` selects a variant.
Its TUI takes no effort option; use `--model provider/model#variant` for an
OpenCode 2 variant instead of a separate `--effort`. Although OpenCode 2's TUI
has no model flag, brgr supports model selection through its native private
server recipe: a temporary loopback server settles the location's catalog,
checks the model and variant, creates a model-selected session, and reads it
back. That server is stopped before the TUI resumes with `--standalone
--session SESSION`. No global or project configuration is rewritten. Missing
models, variants, mismatched read-back, or preparation failures stop the run;
brgr never retries with another model or silently changes to headless. Calling
arguments that override the session, server, or model conflict with this route.

For JSONL OMP process results, brgr checks every assistant event's native
`provider/model` against an explicitly requested selector before publishing a
candidate. `brgr result TASK` exposes that model in a separately committed
`route_observation` receipt; the sealed v1 result JSON and its decision digest
remain unchanged for older binaries. OMP does not expose a
verified effort value in this event stream, so `effort_source` remains
`unavailable`; other process recipes likewise report unavailable native route
identity unless their adapter supplies evidence. A missing or different OMP
model fails closed rather than becoming an accepted candidate.

For lowest available Luna effort on the installed CLIs tested on 2026-09-14:
OMP uses `openai-codex/gpt-5.6-luna` with `--effort low`, GJC uses the same
model with `--effort minimal`, Cursor CLI uses the exact model variant
`gpt-5.6-luna-none` without an effort flag, and Command Code uses
`gpt-5.6-luna` with `--effort low`. Recheck each executable's native catalog
after upgrading it. These are route requests; only OMP and GJC expose a
native model observation, and none expose a verified effort observation.

## Standalone Codex and CLI

### Pane mode

Generated recipes declare native TUI launches for Claude Code, Codex, Gemini
CLI, GitHub Copilot CLI, Pi, Cursor, Devin, Cline, OMP, OpenCode, GJC, and
Command Code. Codex runs every installed hook for a worker as well; brgr's
own hook recognises a task worktree by the session's working directory and
does nothing there, while the user's other hooks still run. Codex has no
"this session only" trust: when a Codex worker starts in a task worktree,
brgr accepts the prompt for the repository that worktree came from, and Codex
saves that repository as trusted in `~/.codex/config.toml`. The registered executable runs in
an owned pane without taking focus. brgr connects parent/worker messages to
that session, seals its final report, and closes the pane after the owner
handles the result. A momentarily idle TUI can continue the conversation.
Owned-workspace trust, "press Enter to continue" notices, update offers (skipped) and an applied self-update are answered automatically; any other screen is reported and fails the run after three minutes. Other native
input uses `brgr input TASK --key KEY` or `--text TEXT`; conversation messages
stay queued while the TUI is busy or displaying a native dialog.

Headless is only ever an explicit per-call choice through `--headless`,
`--mode headless`, or `--mode auto`; auto chooses it only when no Herdr pane is
available and reports that choice. Read-only scope, an unsupported effort,
and legacy `prefer_print_mode` settings never select it. Use
`--owner-session SESSION --source-pane PANE` for a positively verified source
when the caller is a shared daemon. See the [orchestration guide](orchestration.md)
for config, instructions, debate, and the current verification limits.

### Recursive workers

Default TUI tasks own their native pane; an additional supervisor pane is not
created. Set `brgr config set worker-placement tab` for new tabs or `adjacent`
for a split beside the exact caller. Settings live at `$BRGR_HOME/config.toml`
and do not move existing panes. Legacy auto-worker settings remain readable.

A worker receives `$BRGR_BIN`, its exact task/attempt identity, and a scoped
owner session. It may call `brgr run OBJECTIVE` again to create a child; brgr records the
parent edge and delivers the child result to that worker's inbox. The parent
must read and accept/reject a candidate (or acknowledge another outcome)
before its own successful result can become a candidate. `brgr wait TASK`
waits for a child result without approving it. `brgr watch TASK` prints one JSON line per event until every watched task has a result. The `message` subcommands (`send`, `list`, `wait`, `ack`)
let each side ask and answer questions on the active attempt; unanswered
questions block a successful candidate. Standalone runs opt in with
`--enable-delegation`; plugin workers enable it automatically. See
[recursive worker contract](recursive-workers.md) for the verified scope
and remaining gates, including the
[live OMP → GJC → GJC receipt](../evidence/live-recursive-bridge-2026-09-23.md) and
[bidirectional message receipt](../evidence/live-bidirectional-bridge-2026-09-23.md).
Earlier v2.1.0 binaries do not contain these commands.

### Completion loop (v2.3.0)

A Herdr Codex owner receives a stable completion callback when an exact
recorded pane and session become idle; busy or transferred sessions retain a
durable pending notification. A task can carry explicit criteria, scope, and
role instructions. Repeated `--snapshot-path RELATIVE_FILE` captures selected
uncommitted regular files or tracked deletions in the worker worktree while
preserving the source. The captured input is staged only in the worker index;
the source index and files are unchanged. Use `--requires-write`, `--requires-browser`,
`--requires-mcp NAME`, or `--require-capability NAME` to reject unsupported
routes before worker admission.

`--capture-diff`, `--capture-logs`, and `--evidence-file RELATIVE_FILE` seal
bounded review artifacts. `brgr artifact export TASK 0 --output PATH`
exports a result's artifact by index, starting at 0, for binary evidence. `brgr diff TASK` prints a sealed diff and
`brgr diff TASK --stat` lists the files it changes.
`status --observe` does not run the recovery that records a dead supervisor's task as lost: an observer sees what is recorded and records nothing. `--observe` on `status`, `result` and `diff` reads a task owned by another session
(for example an orchestrator's task, reviewed by a person or another agent) without
the owner-session check, and prints a note on stderr that it did. It only reads:
it cannot be combined with `result --ack`, and `accept`, `reject`, `land`, `apply`
and `bind` still need the owner's session. `brgr status TASK --json` prints one object on stdout; every note brgr adds goes to
stderr. Its fields are `task` (the stored task: `task.workspace` is the worktree,
`task.evidence.base_commit` the commit it started from), `state`, `workspace`
and `base_commit` (copies of those two), `workspace_present` (the worktree still
exists), `session`, `calling_options`, `source_pane` and `source_session`. `brgr apply TASK --workspace PATH` checks a sealed
diff; `--execute` applies it only after an explicit accepted result. The diff
includes files the worker created without staging them, except ignored files
and files named with `--evidence-file`. The integration target must be the
repository root at the task's starting commit or a later commit on the same
history, and the diff must still apply cleanly there. When it does not, for
example because parallel workers each appended to the same file, `--3way` merges
it with the target's own changes (`git apply --3way`): a clean merge applies,
and a conflict is written as standard markers, leaves the files unmerged, prints
the files in `conflicts`, records `applied_with_conflicts` in the receipt, and
exits with an error so a script cannot mistake it for success.
`brgr land TASK --workspace PATH --reason WHY` is `diff`'s natural next step: it checks
the sealed diff against the workspace first, and only if that passes records the
accept (which also acknowledges the result, so no `result --ack` follows) and applies
the diff. A diff that cannot be applied records no decision; with `--3way` it
accepts and leaves conflict markers, like `apply --3way --execute`. If the apply
fails after the accept, the error says so and names the `apply` command to retry.
A failed or lost result needs no decision to be revised: a revision accepts it
and counts as the owner's acknowledgement of it. A lost run whose worker may still
be running is refused, with the worker's process named: a second run would share
the task's pid record and hide the first worker from `prune`. `brgr status TASK --tree` shows time and question/approval waits;
`brgr cancel TASK --tree` records cancellation for the subtree before stopping
its active processes. `brgr cancel TASK --force` does the same when this session
is not the bound owner: reaching the control home is the access check. It does
not accept or reject a sealed result; an already-terminal task only closes a
leftover pane brgr recorded as its own. See [the completion loop plan](completion-loop-plan.md)
for the current scope and limits.

Install the local binary and Codex integration, then start a new Codex session:

```bash
cargo install --path crates/brgr-cli --locked --root "$HOME/.local"
brgr integrate codex install
brgr integrate claude install   # same skill for Claude Code
```

Ask Codex, for example, “GJC를 작은 scratch 작업으로 검증해 등록하고,
이 변경을 검토하면서 실패 사례도 확인해줘.” Registration may invoke the
selected harness's model, so authorize its scratch prompt explicitly.
Codex owns the task criteria and the accept/reject decision; brgr owns the
bounded execution, sealed bytes, and durable inbox. The CLI remains an escape
hatch for inspection and explicit operations:

```bash
brgr run "Review this change" --harness local.gjc --criterion "findings cite changed lines"
brgr status
brgr result TASK
brgr accept TASK --reason "criteria verified"
```

If a candidate misses the criteria, use `brgr reject TASK --reason "..."` and
`brgr revise TASK "Corrected request" --criterion "new observable check"`.
The old result and decision remain intact. `brgr result TASK --ack` acknowledges
a failed, cancelled, or lost result; acknowledgment is not acceptance. A clean
Git source gets a dedicated task worktree. Dirty changes are rejected unless
`--allow-clean-head-snapshot` explicitly excludes them.
Git is needed for Git-backed tasks; a non-Git workspace can run without Git.

Codex sessions bind their owner automatically at first task or SessionStart.
A bare CLI run without a session can finish but cannot expose, acknowledge, or
decide its result until `brgr bind TASK --session SESSION` explicitly claims it.
When the owning session changes, hand the head over with the `head` commands
(below) rather than `bind`: `bind` is for an unbound or abandoned owner, and it
is refused while the owner has an open handoff. For a CLI fixture, set both
`BRGR_OWNER_ID=codex:example` and `BRGR_SESSION_ID=example-session` on run and
follow-up commands.

### Handing the head to another session

A handoff moves the owner's binding from the session that holds the head to
another session without losing the owner's tasks, results or decisions. It is a
record in the store with five steps, each its own command. Every command takes a
TASK of that owner, like `bind`.

| Step | Command | Who may run it |
| --- | --- | --- |
| request | `brgr head request TASK --to SESSION` | the current head |
| prepare | `brgr head prepare TAKEOVER --goal TEXT` | the outgoing session, still the head |
| read | `brgr head show TAKEOVER` | anyone with the store (read only) |
| commit | `brgr head commit TAKEOVER` | only the incoming session |
| close | `brgr head retire TAKEOVER` | only the incoming session, after it has resumed |

`request` takes an optional `--key` idempotency key (default: a fresh one),
`prepare` an optional `--notes`, and `retire` an optional `--failed` with a reason
that records a failed close instead of a clean one.

`brgr head abort TAKEOVER --reason TEXT` stops a handoff that has not committed
(either session may run it), and `brgr head status TASK` lists the owner's
handoffs, newest first.

- `prepare` builds the manifest from the store: the owner's open task revisions
  with their attempt, the undecided and unacknowledged results, and the
  unanswered worker messages. Nothing is passed in by hand, and the manifest holds
  references only; it never copies a result or restates a decision. A task
  revision with no attempt yet is not listed; the new head sees it in `brgr status`.
- Preparing a new head and handing over command are different steps. Nothing moves
  until the incoming session runs `commit`, one transaction that re-checks the
  binding, raises the epoch by one and requeues unresolved completion notices for
  the new head. A binding that moved since the request makes `commit` fail and
  leave the handoff `prepared`.
- `resumed` is never reported by a caller. The store records it when the incoming
  session performs an owner-side write at the committed epoch: a decision, an
  acknowledgement, a task admission, a reply or cancel, a debate write. `retire`
  is refused until then, and after a commit the old session is stale for every
  owner command.
- Repeating a command is safe. A repeated `request` with the same key returns the
  same record, even from the outgoing session after the commit; a repeated `commit`
  does not raise the epoch again; a different destination for the same key is a
  conflict.
- While a handoff is open (requested, prepared, committed or resumed) `bind`
  is refused, and only one handoff may be open per owner. Finish it, or abort it
  before the commit.

The epoch and session names are cooperative identity checks between harnesses
run by the same user. They are not a credential and do not authenticate a process.

`brgr integrate codex|claude status|uninstall` checks or removes only brgr-owned
entries; the Claude Code skill goes to `$CLAUDE_CONFIG_DIR` or `~/.claude`. For OMP, Cursor CLI, Command Code, Devin CLI, or an approved unfamiliar CLI, use
`brgr harness draft EXECUTABLE`, `brgr harness test`, then an authorized scratch run with
`brgr harness activate --workspace DIR --prompt PROMPT` and `brgr harness status`. Pass `--model MODEL` when
that exact manifest supports model selection. Do not infer support for flags
absent from the installed executable's help.
See [agent-authored manifests](custom-harness-registration.md) for a
documented CLI whose prompt shape needs a custom declarative recipe.
For an already approved executable, `brgr harness add EXECUTABLE --workspace
SCRATCH --prompt "small authorized probe"` is the combined probe, contract-test,
scratch, activation, and health-check escape hatch. `--presentation-only`
registers the optional Herdr adapter without claiming a managed scratch run.
Process activations created before this scratch requirement remain on disk but
cannot start a new task; re-add each approved executable with an authorized
scratch workspace and prompt. Existing sealed results and decisions are kept.
An exact `--model` additionally requires an activation with a bounded native
model-catalog recipe. Older activations lacking it must be re-certified; an
unknown selector fails before a task worktree or model request is started.
Presentation-only Herdr model requests are checked against OMP's native
catalog before brgr admission and checked again by `omp-role` at dispatch.

When a harness CLI updates, its executable or probe evidence changes and a run
stops with `executable_changed` or `probe_evidence_changed`. `brgr harness
recertify HARNESS` registers it again at the path it was certified with, using
one scratch run (prompt `say ok`, the current directory unless `--workspace`
is given). `brgr config set-auto-recertify true` lets a new run or a revision do
that themselves, in an empty temporary directory, and print a
warning each time. It is off by default because each re-certification is a paid
run that trusts the changed binary; any other health, and a re-certification
that fails, still stop the run. A harness that takes no model per run (Cline,
Devin) refuses `--model` and says how to change its own setting; a successful
run records the model its settings name, as `harness_config` rather than an
observation from the run.

On macOS, `run` and `revise` hold an idle-sleep assertion (`caffeinate -i -w`, tied to
the brgr process, so it ends with it) while they start a task; a machine that sleeps
mid-start wakes with the network down and the model catalog probe fails. A probe
that fails after the wall clock ran 20 s or more ahead of the monotonic clock says the
machine was asleep, and its retry waits longer.

`brgr doctor` probes every registered harness as well as the Codex integration
and store. It exits with `needs_attention` and names an unhealthy harness when
an executable or recipe has changed; re-add that harness with an authorized
scratch run before starting new work. JSONL process capture discards repeated
update events while retaining completed assistant/model evidence. Raw transport
remains bounded to 64 MiB, and the sealed final answer still obeys its manifest
artifact limit. Over-limit processes are stopped and produce a failed result.

`brgr cleanup status TASK` shows whether an owned OMP pane was closed or
retained. `brgr cleanup run TASK` retries a pending close. Neither command
removes a task worktree; `brgr prune` does that separately.

Herdr 0.9.0 accepts `pane.close(pane_id)` without conditional identity fields.
Brgr rechecks owner decision, inbox acknowledgment, pane ID, terminal ID,
immutable agent session, idle state, and protected-tab status immediately
before closing. Another actor could still change the pane between that check
and Herdr's close call. This is a best-effort cooperative-local guarantee, not
an atomic compare-and-close guarantee; use `--keep-pane` for shared sessions.

## Reclaiming task worktrees

Every Git-backed task gets its own worktree, which costs a full copy of the
source working tree and leaves a `brgr/task-*` branch in a repository brgr does
not own.

Since v2.12.0 a decision reclaims its own revision's worktree: `accept`,
`reject` and `result TASK --ack` run the checks below for that one worktree,
and a pane-mode task runs them again once its pane has closed, since its worker
works in the worktree until then. The note "brgr · removed worktree …" or
"brgr · kept worktree …: reason" goes to stderr, and the decision never fails
because of it. `--keep-worktree` on `run` or `revise` keeps it, as does
`--keep-pane`. `brgr apply TASK --workspace PATH` works from the sealed patch, the recorded base
commit and the checkout recorded at admission, so it still works after the
worktree is gone.

`accept`, `reject` and `land` take `--include-ignored`, which also removes a
worktree that only ignored files (build output, caches) kept, as `brgr prune
--apply --include-ignored` would. Only the owner knows those files are
disposable, so without the flag such a worktree is kept. The choice is written
down, so a pane-mode worktree reclaimed later, when its pane closes, honours it.

Each revision is assessed once: a worktree a decision kept is not looked at
again automatically. Whatever a decision kept, and worktrees from before
v2.12.0, are swept by hand:

```bash
brgr prune           # report what is removable and why the rest is kept
brgr prune --apply   # remove them
```

A worktree is only considered once its task revision is settled: the owner
accepted or rejected its candidate, or acknowledged its failed, cancelled, or
lost result with `brgr result TASK --ack`. A failed revision also needs no
attempt still running, no retry granted, and no worker process left alive; a
lost result warns that its worker may outlive its supervisor, and while that
process runs the worktree is kept. The checkout is removed with `git worktree
remove`, so uncommitted work is kept and reported with git's own reason. It is
forced only when brgr has shown every change is disposable: each one is an
untracked harness cache (`.gjc/` or `.commandcode/`, which hold only session
state; `.claude/` or `.cursor/` also hold project configuration a task may
write, so they are not caches), or the task sealed a diff with
`--capture-diff`, the worktree's patch is byte-for-byte that diff, and each
`--evidence-file` still has the digest of its sealed copy. A repository nested in
the worktree, which a patch records as one commit id, and a `.brgr/` report
directory that was not archived keep the worktree in every case. Its `brgr/task-*` branch goes only when every commit on it also lives
elsewhere: in `HEAD`, or on another branch or remote-tracking branch. A task
started from a feature branch carries that branch's commits, and those are
safe while the feature branch exists. Other task branches do not count, since
the same sweep may delete them. A branch holding a commit found nowhere else is
kept, with that reason.

git is not the only check. Its clean test does not look at ignored files, so
`git worktree remove` would silently delete an ignored `.env`, a downloaded
credential, or a build cache — the things a task worktree is most likely to
hold. Those worktrees are kept and their ignored paths listed; pass
`--include-ignored` to remove them anyway. An ignored directory a tool owns
and recreates in full (`node_modules`, `target`, `.next`, `.turbo`,
`__pycache__`, `.pytest_cache`, `.mypy_cache`, `.venv`, `.gradle`) does not keep
a worktree when git reports the directory itself as ignored. Generic output
directories such as `dist` or `build` do: when the directory matches an ignore
pattern git never lists what is inside, and a file put there by hand would go
with it. A symlink, a directory git has not
registered as a worktree, one git has locked, a name the layout could not have
produced, and the directory prune is running in are all refused.

A sweep also reclaims the `brgr/task-*` branches left behind by worktrees that no
longer exist, which is what removing one by hand leaves in a repository brgr does
not own, under the same rule: a commit on no other branch keeps its branch. One
unreadable directory is reported rather than ending the sweep.

Sealed results, decisions, artifacts, and task rows are never removed — they
cost about 9 KiB per task, while a worktree costs the size of the checkout.
Pruning is not owner-scoped, because cross-owner isolation is outside v1 scope;
each row reports its owner. `brgr prune` is rejected through the Herdr host
bridge; run it explicitly outside the plugin Codex pane.

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -p brgr-cli -- --help
```

The local store defaults to `~/Library/Application Support/brgr`. Harness
output, help text, manifests, and reports are treated as untrusted data.
Before upgrading, retain a recoverable copy of the private registry and store.
An older `v1.0.5` binary cannot load a newly activated manifest containing
`model_catalog`; this fails closed without deleting results. Restore a matching
registry snapshot only after stopping new admissions and confirming no active
task depends on the newer activation. Never overwrite a newer live store with
an old snapshot.

See [Architecture](architecture.md) for the managed-run contract,
and [the documentation index](../README.md) for the guides, release notes, and
the dated evidence records behind the claims made here.
