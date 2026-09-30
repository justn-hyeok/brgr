# Changelog

## 2.9.2 — 2026-09-30

The Codex Stop hook now blocks as intended when brgr has unprocessed results.
Its output carried a `hookSpecificOutput` object, and Codex defines none for
Stop, so Codex rejected the whole output as "invalid stop hook JSON output" and
let the session stop anyway. The hook now emits only `decision` and `reason`,
and the reason names the pending tasks.

## 2.9.1 — 2026-09-30

A pane-mode run now completes as soon as the agent is idle and its report
exists. It also required having seen the agent working, and Herdr may never
show that: running three pane tasks at once, Cursor went from unknown straight
to idle after finishing between two polls, and brgr waited on a finished task
until its deadline. An agent never seen working that sits idle without a report
for a minute is reminded once, as one seen working already is after 15 seconds.

A cancelled or killed pane-mode run no longer leaves its pane open with the
agent still in it. The runner records the pane it opened, and the supervisor
closes a recorded pane once the attempt is over, whatever ended it. A run with
`--keep-pane` keeps it. Such a run is still recorded lost, as every
Herdr-backed run stopped early is.

## 2.9.0 — 2026-09-30

Cline no longer takes `brgr run --model`; it runs its configured model, as
Devin does. Cline documents `--model` as applying to one session, but Cline
3.0.65 saved an unknown name as its provider default: every later Cline run,
inside brgr or not, failed with "model not found" until the setting was fixed
by hand. brgr does not change a CLI's own configuration, so it no longer passes
a model to Cline at all. A task that asks for one is refused before it runs.
Re-register Cline to pick up the change.

Pane mode now covers Cursor, Devin, Cline, OMP, and OpenCode as well as Claude
Code: inside Herdr each runs as its own interactive session in a pane beside
the caller. Every one was started in a real Herdr pane and completed a task,
with its report sealed and its pane closed, before its recipe gained the
launch. Cursor starts with `--trust` and Devin with
`--respect-workspace-trust false`, as their print modes already do. OpenCode's
TUI takes no effort flag, so an OpenCode task with `--effort` runs headless
rather than without the effort; a new manifest field,
`launch.interactive.effort_print_only`, records that. GJC and Command Code are
not agents Herdr recognizes and stay headless. Re-register a harness to pick
up its interactive launch.

A pane-mode agent that starts on a screen Herdr cannot classify — Cline opens
with a product notice waiting for a key — no longer fails the run after a
minute. The owner gets a question naming the pane, and the run waits for the
agent to be ready.

A question a pane-mode run sends its owner is now withdrawn once the agent is
ready again. It used to stay unanswered after someone dealt with the prompt in
the pane, and brgr then failed the finished task with "question(s) remain
unanswered". Only the run that asked a question can withdraw it, and a
withdrawn question is no longer pushed to the owner.

Harness registration's scratch run may now take three minutes instead of one.
A free model behind a queue took 89 seconds to answer a one-line prompt, so a
working harness could not be registered.

## 2.8.0 — 2026-09-30

`brgr prune` now deletes a task branch when every commit on it also lives on
another branch or a remote-tracking branch, not only when it is merged into
`HEAD`. A task started from a feature branch carries that branch's commits, so
once you went back to `main`, `git branch -d` called its branch unmerged and
kept it forever. Other `brgr/task-*` branches do not count as a copy, since the
same sweep may delete them, and a branch holding a commit found nowhere else is
still kept. The deletion still refuses a branch some worktree has checked out,
and a branch that moves while it is being deleted is restored.

## 2.7.1 — 2026-09-30

A harness's version, help, and model-catalog probes may now take 15 seconds
instead of 5 before brgr reports the harness timed out. The limit only exists
to catch a hung CLI, and a healthy one answers in about a second, but an
ordinary stall on a busy machine pushed a working harness past five seconds;
`brgr run` then refused the task as `timed_out`, or rejected a valid model
request with a catalog-probe failure instead of checking it. The same stall
made a contract test fail intermittently after fresh builds.

## 2.7.0 — 2026-09-30

`brgr prune` now reclaims the worktrees of failed, cancelled, and lost runs.
It required an owner decision, and only a candidate can be accepted or
rejected, so every failed run's checkout stayed forever. A failed revision now
counts as settled once its owner acknowledged the result (`brgr result TASK
--ack`), no attempt of it is still running, no retry was granted, and no worker
process is left alive — a lost result's harness can outlive its supervisor, so
brgr checks the supervisor and the harness process before removing anything.
A kept failure says which of these is missing, including the `--ack` command
to run.

## 2.6.1 — 2026-09-30

A task placed in a Herdr worker pane now has a minute, not five seconds, to be
claimed. Herdr has to open the pane and start `brgr plugin worker` in it before
the task's supervisor exists, and any brgr command run in the meantime treated
the task as abandoned after five seconds and recorded it lost, with its worker
still on the way. The launch envelope now records what will claim the task
(`claimant`); a detached supervisor keeps the five-second window, and envelopes
written before this change keep it too.

## 2.6.0 — 2026-09-30

Pane mode now waits for a newly split pane's shell before starting the agent.
Herdr refuses an agent until the pane shows its prompt, and a slow shell
startup failed the run within a second. A pane-mode run that fails now closes
the pane it opened, and a lost Herdr-backed run reports the adapter's own error
(for example "Herdr could not start the claude agent: …") instead of a generic
message that named OMP for every harness.

The Codex skill that `brgr integrate` installs now covers what brgr gained since
it was written: the registered harnesses, `--permission` levels and the
permission cap, model selection, pane mode and how to handle a blocked pane
(inspect it, then ask the user), `brgr diff`, apply onto later commits, and
`brgr prune`. A new `brgr-diff-review` skill in `skills/` walks through
requesting, reviewing, deciding on, and integrating a worker's sealed diff.
Run `brgr integrate codex install` to update an installed skill.

`brgr diff TASK` prints a result's sealed diff, and `brgr diff TASK --stat`
lists the files it changes with added and deleted line counts. They show the
same bytes `brgr apply` writes, so review and integration cannot disagree;
before this, reading a diff meant exporting it to a file first. `--json`
returns the patch, the per-file counts, and the patch digest together.

`brgr apply` now accepts a target whose `HEAD` has moved past the task's base
commit, as long as the new commit descends from it and the diff still applies
cleanly. It used to require `HEAD` to be exactly the base, so any commit made
while a worker ran blocked integration. A target on unrelated history is still
refused, and the result names both commits (`base_commit`, `target_head`).

A sealed diff (`--capture-diff`) now includes files the worker created without
staging them. It used to compare only paths the worktree's index already
tracked, so a new file fell out of the patch without any error, and `brgr apply`
integrated an incomplete change. brgr now marks untracked, non-ignored files
intent-to-add in a throwaway copy of the worker's index before diffing; the
worker's own index is not written. Files requested with `--evidence-file` stay
out of the patch, since they are evidence for the owner rather than changes to
apply.

Inside Herdr, a worker can now run as the harness's own interactive session in
a pane beside the caller, so the work is visible while it happens. A harness
opts in with a new manifest field, `launch.interactive` (`herdr_kind` and extra
`argv`); Claude Code is the first. brgr splits a pane to the right without
taking focus, starts the agent there with the task's permission, model, and
effort flags, sends the prompt, and asks the agent to write its final answer to
a report file, which brgr seals as the result. The pane closes when the task
finishes. If the agent stops at an approval or question prompt, brgr sends the
owner a question notice instead of answering it. A `read-only` task, a task run
outside Herdr, and a harness without the field all keep the print-mode run.
`brgr config set-pane-mode false` turns pane mode off.

Claude Code and Cline can now run a chosen model with `brgr run --model`. brgr
checks a model name against a list before a paid run, and neither CLI can list
its models, so both used only their configured default. Both refuse a name they
do not know on their own, locally and before any request — Claude Code in about
three seconds ("isn't described by this version's model catalog"), Cline in about
four ("model not found") — so the check is theirs to make. A new catalog format,
`cli_validated`, records that: it names no list command, and a harness gets it
only where that refusal was observed. An empty name and `auto` are still refused
by brgr before anything runs.

## 2.5.0 — 2026-09-29

Workers now run at a permission level: `full`, `edits`, or `read-only`. Each
harness recipe maps the levels to its own flags — Claude Code
`bypassPermissions`/`acceptEdits`/`plan`, Command Code `yolo`/`accept-edits`/
`plan`, Devin `dangerous`/`accept-edits`/`auto`, Cursor `--force`/agent mode/
`--mode plan`, OMP `--approval-mode=yolo`/`write`, Cline `--auto-approve true`/
`--plan`, OpenCode `--auto`/`--agent plan`. `full` is the default. Choose per task
with `brgr run --permission` or `brgr revise --permission`, and cap every task
with `brgr config set-max-permission`: a request above the cap, or above the
parent task's own level, is refused rather than lowered, and a level a harness
cannot honour is refused before anything runs rather than run under a wider
one. A task that asks for nothing and has no cap runs exactly as before.

This changes two recipes' defaults. Cursor no longer pins `--mode ask`, and
Command Code no longer pins plan mode and two turns; both were unable to do
real work. Re-register a harness to pick up its new recipe.

New recipes for Claude Code (`local.claude-code`), Cline (`local.cline`), and
OpenCode (`local.opencode`). Claude Code and Cline have no model list to verify a
name against before a paid run, so they use each CLI's configured default model,
as Devin does; effort remains selectable. OpenCode's models are verified
against `opencode models`, read by a new one-selector-per-line catalog format.
Claude Code needs `USER` to find its keychain login, so it is allowed through.
Help printed only to stderr — as `opencode run --help` does — now counts as
help, in registration and in health checks alike.

A failed registration now says why: exit code, whether it timed out or was
truncated, and how much it printed. Before, a working CLI whose account had run
out of credits looked broken with nothing to go on.

A notification dispatcher now stops when its control home is removed. It polls
for up to a day, and an open database connection keeps reading a deleted file,
so every test run that deleted its home left dispatchers running; a single
session of this project's own test runs had left fourteen. Production homes are
not deleted, so installed use is unaffected.

## 2.4.0 — 2026-09-29

Every managed worker can now ask its owner a question. The identity a worker
needs to message — and the brief that tells it how — used to be attached only
when a task was started with delegation, so a plain `brgr run`, the route the
orchestration skill uses by default, produced workers that could not ask at all;
the author's store held zero messages across 57 tasks. A worker without
delegation now gets the owner-messaging brief appended after its task, so the
task's own first line is unchanged (an OMP objective must begin `FROM CODEX`).

Delegation stays a permission. Carrying an identity also lets a worker name
itself as a parent, so child admission now checks that the parent task was
started with delegation, against the launch record it was started from, and
refuses the child otherwise. Both behaviours are tested with a real worker
process rather than a simulated identity.

A worker's question now reaches its owner. Only completions were ever pushed to
the owner's Codex pane, so a question sat until the worker's own `message wait`
timed out unless someone ran `brgr status --tree`. The notification dispatcher
that already runs for the life of a task now also sends a `FROM BRGR` notice of
type `brgr_question`, carrying the `message_id` and the exact commands to read,
answer, and acknowledge it. Like a completion it waits until Codex is idle. A
new `question_notices` table records which session was told, so a question is
announced once per session: a restarted dispatcher does not repeat it, a
transferred owner is told again, and a reply ends it. The guidance brgr installs
into Codex describes the notice.

A harness manifest whose result source says `stdout` but carries a `path` is now
refused. It used to parse as `Stdout` with the path silently dropped, so a
manifest meant to read a result file sealed raw stdout instead and skipped every
file-result guard — the relative-path check, symlink refusal, size limit, and
device and inode re-check. Adding `deny_unknown_fields` was not enough: on an
internally tagged enum it does not reach unit variants, which still parsed. The
source is now read through a struct of every permitted key and converted, so an
unknown key, a `path` on a kind that takes none, and a `file` without one are all
errors. All four manifests installed on the author's machine carry only `kind`.

A contended terminal commit now hashes its artifacts once. Verification had
been moved out of the write lock but left inside the retry, so each contended
attempt re-read and re-hashed up to 20 MiB and discarded it. The retry-decision
guard now compares calls with whitespace removed, since rustfmt moving a long
call into a block had made it report a retried path as unretried.

The registry's crate documentation said `Registry::health` re-checks probe
evidence. It re-digests the executable only; `health_probed` re-runs the probes
and is the only source of `EvidenceChanged`. Task admission already used the
probed check, so behaviour was right and the documentation was not.

`brgr prune --apply` could delete branches in a repository brgr had never
worked in. It learned which repository a worktree directory belonged to by
asking whichever child directory sorted first, without checking that the child
was one brgr created; a checkout of another repository dropped there by hand
sorts before any lowercase slug when its name starts with a capital letter or
punctuation, and the sweep then ran `git worktree prune` and `git branch -d
brgr/task-*` against that repository. Reproduced on git 2.54.

Repositories now come from the store. The task spec records the worktree brgr
created, not the repository it came from, so admission now records the primary
checkout too (`task_checkouts`, a new table; an existing store gains it on first
open). A candidate is judged only after the store confirms brgr created that
exact path for that revision, and against the repository recorded for it.

Four related gaps in the orphan-branch pass are closed with it. Orphans were
never looked for when every worktree of a repository had been removed by hand —
the case the pass exists for — because the repository was discovered from a
surviving worktree. `git worktree prune` is gone: it is repository-wide and would
also drop the registration of a user's own worktree on an unmounted volume, so
the task's own stale registration is now removed by path. An orphan branch now
needs what a worktree needs — a task revision of that repository and a recorded
decision — and reports its owner. And report mode now asks git's merge question
before promising `removable`, so an orphan with commits is `kept` in both modes.

Tasks admitted before this change are still located through a surviving worktree
and git's stale registration. Where neither exists, the directory is reported as
a repository that cannot be located, rather than counted as clean.

`every_write_path_has_a_recorded_retry_decision` was checking far less than it
claimed, in both of its halves. The implicit-write detector compared a whole
untrimmed line to `self.connection`, which no indented line can equal, so it
matched none of the six implicit write paths; a first repair matched only lines
starting with it and still missed five, because rustfmt writes most of them as
`let changed = self.connection...` or splits `self` and `.connection` across
lines. The declaration check read only `lib.rs` and skipped any name it could not
find there, so the module split had silently removed nine of twenty-one
declarations from it. It now reads every scanned module and fails on a declared
name defined nowhere. Checked by mutation in four shapes: a flipped declaration,
a misspelled one, and an unclassified write in three syntactic forms.

Three tests passed for the wrong reason. The core doc example asked revision 1 to
accept revision 4 and called the refusal "revision 3 was skipped" — it failed
because 4 is not 2, and would have for any number. The runner's model assertion
searched for the joined `provider/model` form, which the stream never contains
contiguously, and no generated case carried an identity, so it never ran. And
the protocol example attributed its byte-level difference to omitted optional
fields when the actual cause is pretty-printed versus compact encoding.

`docs/` is sorted into four directories and carries an index. It was forty-seven
files in one flat listing, nineteen of them release notes, with nothing saying
what was current and what was a dated record of one afternoon in September. The
split is by how each kind of document ages: `guides/` is edited as the code
changes, `releases/` is written once per version, `evidence/` is never edited
after the date in its name, and `readiness/` holds the checklist. A guide that
disagrees with the code is a bug; an evidence record that disagrees with it is
simply older, and editing one destroys what it was for.

The move rewrote sixty-seven links, so it comes with a test that resolves every
relative link in the repository's prose — markdown links, and the bare
documentation paths that appear unlinked in Rust strings and workflow steps,
where a link checker would not normally look. It found three links in the readiness checklist that
broke because the file dropped a directory level, and it refuses to pass if the
extraction stops finding links at all.

Every crate root now carries a worked example, and `cargo test --doc` is no
longer a step that cannot fail. CI has been running it against zero doc tests,
so a green result meant nothing; there are now seven, and each was checked the
same way the property tests were — by deleting the rule it covers and confirming
the example fails.

That check changed two of them. `Attempt::transition` and `Store::claim_attempt`
both originally asserted `is_err`, which held for the wrong reason: a step from
`Running` to `Terminal` is not a legal edge either, and an unrecognized prior
outcome is refused by the same fallback. Both now name the variant they mean. A
third, the manifest example, was rejecting its input on an unrelated malformed
field rather than on the misspelled key it was meant to demonstrate, and now
carries a control that parses.

What the examples document is the rule each crate exists to enforce: the task
revision chain and the attempt state machine in `brgr-core`, the wire schema and
its refusal of an unknown version in `brgr-protocol`, `deny_unknown_fields` and
the shell-free argument vector in `brgr-runner`, the stable health code and the
POSIX-quoted remedy in `brgr-registry`, and in `brgr-store` the candidate rule
that `brgr prune` depends on for its claim that a settled worktree holds no
running attempt.

Three parsers that read bytes brgr did not write now carry property tests: the
harness JSONL stream, the Herdr bridge request file, and the directory names under
the worktrees root. Each generator is deterministic and seeded, so a failure names
the case and re-running reproduces it, and each property was checked by deleting
the rule it covers and confirming the test fails. Two rounds of that check found
generators that never reached the accepting path at all — a bound of roughly a
megabyte that six-kilobyte inputs could not press on, and eleven of four thousand
slugs parsing — so both tests now count how often they reach it and fail if that
count collapses.

The slug property found a real gap. `parse_slug` accepted four families of
directory names brgr never creates: `-r+5` and `-r007` and `-r01`, because Rust's
integer parser takes a leading sign and leading zeros, and a plain `-r1`, because
`task_slug` writes revision one with no suffix at all. Each resolved to a live
revision, and what follows an accepted slug is `git worktree remove` and a branch
name rebuilt from the raw text. A suffix must now re-render to exactly what was
read.

The bridge property found dead code rather than a defect. The 64 KiB ceiling on a
request's combined arguments is unreachable: an encoded request contains its own
arguments, so while the file bound is no larger, an oversized command is rejected
as a file first. The ceiling is kept as the bound that still holds if the file
bound is raised, is named rather than spelled inline, and a test now records which
of the two is doing the work — and fails if that changes.

Retry coverage is now a recorded decision per write path rather than a claim. The
store has twenty-one write paths — fifteen opening an explicit transaction and six
writing through an implicit one — and four were retried. The five a running attempt
depends on now are: the launch receipt, the runner identity, attempt state
transitions, the pre-spawn retry grant, and supervision events. Losing one of those
to contention leaves a paid run unfinished for recovery to settle as `Lost`, which
no later attempt on that revision can supersede.

The rest are left alone on purpose. Task admission must fail fast because it holds
the repository admission lock. A command the caller can simply reissue should not
occupy a runtime worker waiting. Notification delivery carries its own claim and
lease protocol that already re-drives a lost step.
`every_write_path_has_a_recorded_retry_decision` fails both when a write path has
no decision and when a declared decision disagrees with the code.

A schema stamp from a different build no longer makes an open take the write lock.
The stamp is derived from the schema text, so two builds whose text differs carry
different stamps, and re-applying the batch on that signal alone had them rewrite
`user_version` past each other on every open — turning a read-only command into one
that needs the write lock. A mismatch is now checked read-only first, against the
object names read out of the schema text, and the batch runs only when something is
really absent. Two builds that disagree only on the digest leave it alone.

A terminal commit no longer holds the store's write lock across artifact file I/O.
`verify_candidate_artifacts` reads and re-hashes every sealed artifact, up to the
20 MiB a manifest may declare, and moving the transaction to an immediate begin had
put that read inside the lock — so concurrent commits queued behind each other's
hashing. The verification now runs first, against a task spec read without the
lock, and the transaction compares its own copy of that spec before inserting.
`tasks.spec_json` is insert-only, so the two cannot disagree; a mismatch is a
typed error rather than a silent difference.

`brgr prune` gains four corrections found by reproducing a review of it. A
worktree git has locked is now refused in report mode as well, so `removable` no
longer promises a removal that `--apply` then declines. Branches left behind by
worktrees removed outside brgr are reclaimed, since the sweep previously walked
only directories that still existed and could never see them; `git worktree prune`
drops the stale registration and `git branch -d` still refuses unmerged commits. A
directory that cannot be read is reported as one kept row instead of ending the
sweep and hiding every worktree it could have reclaimed elsewhere. The repository
worktree listing is read once per repository rather than once per candidate, and
one status call now answers both the clean check and the ignored set, which takes a
report over a hundred settled worktrees from roughly five hundred git invocations
to about a hundred.

Narrow a P0 defect in which concurrent admissions failed with a raw
`database is locked` error after their harness had already run. A deferred
transaction that reads before it writes reports `SQLITE_BUSY_SNAPSHOT` in WAL
mode, which `busy_timeout` does not cover. 2.2.0 converted ten store write paths
to an immediate begin; `commit_terminal_result_guarded` was the one it missed,
and it is the most expensive place to fail, because the harness has already run
and recovery settles an unfinished attempt as `Lost` with unresolved effects that
no later attempt on that revision can supersede. It now begins immediately.

Every write transaction in the store crate now begins through one helper, so
`a_write_transaction_takes_its_lock_at_begin` covers all sixteen rather than the
two that happened to use it, and
`every_write_transaction_begins_through_one_helper` fails if a new one is built
inline. That second test exists because the first version of these notes claimed
the gate covered every path while it covered two: a bench binds a performance
claim to a measurement, and nothing bound this coverage claim to anything.

`Store::open` also wrote `PRAGMA journal_mode` and re-applied the schema batch on
every command, so simultaneous opens contended before doing any work; the journal
mode is now only written when it differs and the DDL batch is gated on a stamp
derived from the schema text. Lock contention is retried under a bounded budget
at `Store::open` and at five write entry points — task admission, attempt claim,
terminal commit, and both decision paths. The remaining write paths still surface
a busy error to the caller.

Sealed results, decisions, and inbox items were never lost by the defect, and the
fix adds no new behavior to that path.

Add `(task_id, revision)` indexes on `results` and `attempts`. The Herdr board
projection joined both tables without one, which made a two-second refresh cost
3.26s at 6,400 stored tasks; it now costs 11.5ms, and its per-task cost falls
rather than grows as the store fills. Existing stores gain the indexes on the
next open and keep every row. Deriving the stamp from the schema text rather than
a hand-maintained constant means adding an object can no longer be forgotten and
silently skipped by every existing store.

Add `brgr prune`, which reports and with `--apply` removes task worktrees whose
revision carries a recorded owner decision, together with the `brgr/task-*`
branch each one created. Nothing in the store is removed: a worktree costs the
size of the checkout while a task's rows cost about 9 KiB.

Removal uses `git worktree remove` and `git branch -d` without forcing, but git
is not the only safety authority. Its clean check runs `git status --porcelain`
without `--ignored`, so an ignored `.env`, credential, or build cache is
invisible to it and would be deleted silently; prune checks the ignored set
itself and keeps the worktree unless `--include-ignored` is passed. A candidate
must also be a real directory rather than a symlink, a worktree git has
registered for its repository, a name the worktree layout could have produced,
and outside the current working directory. A checkout whose branch git declines
to delete reports `removed_branch_kept` and counts as reclaimed. Report and
apply run the same checks, so `removable` in one is removed by the next. The
command is rejected through the Herdr host bridge.

Document every CLI subcommand and global option in `--help`, which previously
listed thirteen commands with no descriptions. Describe the legacy
`route_observation` field in `schemas/result-v1.json`, which a closed schema
rejected although brgr still reads and re-serializes those bytes.

New coverage: an eight-way concurrent admission gate over a real Git repository
(a non-Git workspace takes no admission lock and creates no worktree, so it
cannot observe this contention), a deterministic gate that fails if a write
transaction stops taking its lock at `BEGIN`, structural drift checks
between `schemas/` and the serialized wire types, the 64 MiB JSONL transport
bound, store schema migration and index plans, and prune's keep-and-report
behavior. New benches report board projection cost against stored task count
and concurrent admission efficiency against failure rate. CI gains a
`cargo-deny` supply-chain job.

## 2.3.1 — 2026-09-25

Allow the Herdr plugin's Codex pane to use an explicitly configured absolute
Codex executable when the Herdr server has a minimal `PATH`. Before delivering
a completion notification, brgr verifies the exact Codex pane and reports its
bound session to Herdr when native session metadata is absent. Registration
runs after task supervision starts, so a slow Herdr socket cannot make a task
look unclaimed. Herdr lookup in Codex hooks now has a short budget and never
hides inbox or Stop-hook output when the socket is slow or unavailable. No
owner decision or harness fallback occurs implicitly.

## 2.3.0 — 2026-09-24

Complete the managed delegation loop. Exact Herdr Codex panes receive durable
completion notifications after they become idle, including session transfer;
the owner still verifies and explicitly decides each result. Worker prompts can
carry acceptance criteria, scope, and role instructions. Selected uncommitted
files can be copied into isolated task worktrees without changing the source.

Capability requirements now fail before task admission when the registered
harness cannot support the requested work. Bounded diff, log, and file evidence
can be sealed and exported; requested logs survive failed and cancelled runs.
Accepted tracked-file patches require a separate conflict-checked apply. Tree
status exposes waits and remaining time, and
subtree cancellation prevents new children and stops active process workers.
Retrying children retain their concurrency slots. These additions preserve the
explicit decision boundary and do not introduce implicit harness or model
fallback.

## 2.2.2 — 2026-09-24

Preserve a delegated child's exact parent edge when a rejected result is
revised. The parent attempt must still be active; the revised child may use
its parent's Git task worktree without crossing the control-home boundary.
Allow the exact worker attempt to read and acknowledge existing mailbox
messages after it becomes terminal, while new messages still require an
active attempt. These fixes do not change task/result wire formats, harness
routing, or explicit acceptance.

## 2.2.1 — 2026-09-23

Allow an explicit `herdr.auto_worker_pane` setting to open the brgr worker
pane for detached managed runs from ordinary Herdr panes. It defaults off to
preserve standalone CLI behavior without the plugin. The brgr plugin's Codex
pane continues to open a worker automatically. The adjacent/tab placement,
exact caller pane, task receipt, and explicit owner decision stay in brgr.
Update the installed brgr Codex skill to route bounded harness work through
that path and describe recursive child settlement and bidirectional messages.
A deterministic fixture launched from an ordinary Herdr shell pane reached a
sealed result and explicit acceptance in an unfocused adjacent worker pane.

## 2.2.0 — 2026-09-23

Add bounded recursive worker delegation across registered process harnesses.
Each child is bound to its exact parent attempt, and each parent must settle
its children before producing a candidate. Add an attempt-scoped question,
reply, and acknowledgment mailbox in both directions; unanswered questions
block a successful candidate. Herdr opens worker panes beside their caller by
default, with a control-home setting for separate tabs.

The local Herdr 0.9.0 OMP → GJC → GJC smoke completed a single accepted
three-level chain with twelve acknowledged messages. The process-worker pane
is not an interactive GJC TUI. Deep splits can become narrow, and arbitrary
dirty worktree, cancellation, and restart scenarios are not covered by this
live receipt. The CLI bridge now handles long waits alongside replies and
cancel requests, and message wait tolerates the gap before attempt creation.

## 2.1.0 — 2026-09-22

Declared every remaining public v1 readiness gate closed and added regression
coverage for ENOSPC seal/commit behavior, OMP identity-swap fail-closed,
duplicate-completion idempotent replay, delegated cancel/deadline `lost` paths,
and the pane-cleanup status matrix. A flaky bridge timing test was fixed. No
execution contract, adapter behavior, wire format, or store migration changed.

That gate declaration did not hold: see the 2.2.0 entry and the
[reopening record](docs/readiness/v1-readiness-checklist-2026-09-14.md#2026-09-28-게이트-재개-기록).

## 2.0.2 — 2026-09-15

Add a generated `local.devin` process recipe for Devin CLI. It uses documented
prompt-file print mode, smart permissions, and an explicit non-interactive
workspace-trust override, then seals nonempty stdout through the existing v1
result and Codex decision contract. Devin model and effort selection remain
unsupported through brgr; the recipe uses Devin's configured default and fails
instead of probing its current 64 KiB-plus model catalog or guessing a variant.

The installed Devin 3000.10.27 path completed an authorized scratch run and a
fresh managed run through sealed artifact, durable owner inbox, and explicit
acceptance. This adds no shell execution, implicit model fallback, automatic
acceptance, or store/wire migration.

## 2.0.1 — 2026-09-15

Harden the v2 personal-use path around the Herdr host bridge, Git worktree
admission, harness health checks, SQLite writer contention, and the read-only
board. The plugin Codex pane now exposes only an ephemeral bridge directory;
the host pins accepted brgr commands to the configured control home and
selected workspace and rejects registry or integration mutation. Bridge
timeouts and output overflow stop the spawned process group, and encoded
responses are bounded before publication.

Concurrent admissions share a repository-wide lock across linked worktrees,
preserve collisions, and release the lock before foreground execution. Harness
health distinguishes executable, spawn, timeout, exit, and probe-evidence
failures and detects current Codex hook or skill drift. The board reads an
existing store without schema mutation and validates relational task, result,
and decision identities before projecting them. No wire format, automatic
acceptance, implicit fallback, or public v1 readiness claim changes.

## 2.0.0 — 2026-09-15

Package brgr as a Herdr 0.9+ plugin on macOS. Workspace actions open a
read-only task board, launch a worktree-bound Codex pane, and run the existing
health check. The Codex pane installs the brgr-owned integration and uses the
plugin-built binary. A private, pane-lifetime bridge runs brgr commands in the
Herdr host while Codex keeps its command sandbox. Herdr supplies execution and
status presentation; Codex retains the explicit final accept/reject decision.

The v1 managed-run CLI, store, result envelope, and decision contract remain
available without Herdr. This release adds no implicit harness or model
fallback, automatic acceptance, store migration, or worker cancellation claim.
The optional legacy OMP-through-Herdr adapter keeps its documented cleanup
limit. See `docs/guides/v2-herdr-plugin.md` for the plugin contract and evidence gates.

## 1.0.9 — 2026-09-14

Harden the local managed-run path for personal use. `brgr doctor` now checks
every registered harness and the Codex integration, returns `needs_attention`
for missing or drifted paths, and exits unsuccessfully until they are healthy.
This changes the exit status of an unhealthy doctor check.

JSONL process capture now discards repeated update events while retaining
assistant completion and model evidence. It bounds raw transport to 64 MiB,
keeps the final artifact limit, and stops the process group promptly on
overflow. A fresh Codex session and the installed GJC and OMP process routes
completed sealed-result, owner-inbox, and explicit-decision checks. No wire or
store schema, implicit model fallback, or automatic acceptance changed.

This is the last planned `1.0.x` personal-use stabilization patch. The separate
public v1 completion checklist remains `NO-GO`.

## 1.0.8 — 2026-09-14

Publish the current v1 readiness checklist with linked implementation, live
harness, crash-window, and unsigned-release evidence. The README now points
to the open practical-use gates. No execution contract or adapter behavior
changed; this is a public documentation and version-alignment patch, not a
claim that every v1 acceptance gate has passed.

## 1.0.7 — 2026-09-14

Command Code registration now enables `--effort` only when its installed help
documents that option. A requested effort is passed as a separate argv value;
older executables without the flag remain explicitly unsupported. Live
minimum-effort Luna runs across OMP, GJC, Cursor CLI, and Command Code are
recorded in `docs/evidence/live-four-harness-luna-min-2026-09-14.md`. Cursor's `none`
level is selected through its exact model variant, not an invented effort flag.
Native effort remains unobserved; accepting a response does not prove that a
provider honored an effort setting.

## 1.0.6 — 2026-09-14

Named process harnesses no longer activate through help/version probes alone.
`harness add` requires an explicitly authorized scratch workspace and prompt,
runs the observed contract and scratch task, then checks health. Scratch
workspaces overlapping the brgr control directory are rejected. Older process
activations without a scratch receipt require re-certification before new task
admission; stored results and decisions are unchanged. The optional Herdr
adapter is explicitly presentation-only and does not claim this certification.

Release SBOM generation now targets the `brgr-cli` Cargo package, records
metadata-derived dependency license declarations, and fails if any package
license is unasserted.

OMP JSONL process candidates now require native assistant `provider/model`
evidence to match an explicitly requested model; missing, mixed, or changed
models fail before artifact sealing. A separate atomic receipt records the
observed model and explicitly marks effort evidence unavailable; the v1
result envelope and decision digest remain compatible with older binaries.
Results written by an unreleased intermediate embedded-field build remain
readable without rewriting their stored bytes or decisions. Replayed decisions
return the persisted decision ID, not a newly generated uncommitted one.

Exact model requests now run a bounded, declarative native catalog preflight
before task worktree creation and before paid scratch. OMP, GJC, Cursor CLI,
and Command Code packages supply their respective catalog shapes; agent-authored
packages may declare the same generic formats. Missing or unknown selectors
fail closed. The optional Herdr adapter also checks OMP's catalog before brgr
admission. Native probes retain original file descriptors and immediately
stop their process group after a 64 KiB output threshold (sampled every 1 ms) or a
finite deadline; trusted executables can briefly overshoot that disk threshold
between samples. New activation manifests contain `model_catalog`; `v1.0.5`
binaries reject those manifests on downgrade, so retain and restore a matching
registry snapshot only at an idle boundary. Result and decision bytes remain
compatible and must not be overwritten by an old store snapshot.

## 1.0.5 — 2026-09-14

Fix a Herdr-backed OMP revision replay: a new task revision could reuse the
previous revision's report while its agent was still at startup idle. The
wrapper now uses a fresh revision-scoped report and agent identity, refuses
existing report bytes, and requires an observed lifecycle transition from the
same pane, terminal, and immutable agent session bound to the spawn receipt
before publishing a
candidate. Tests and an actual WorkBuddy/DeepSeek R1→R2→R3 run cover the
negative replay and corrected positive path. A detached successful run also
has a durable offline-owner inbox regression test. Recovery now defers to a
live task-bound supervisor during the brief identity-recording window and
ignores a stale observation when a concurrent runner advances first. A
replacement supervisor reconciles old attempts before publishing its own
receipt, so it cannot adopt a dead predecessor. Process-level tests exercise
both sides of that race. All
previous unsigned and best-effort Herdr cleanup limits remain.

## 1.0.4 — 2026-09-14

Process-level crash fixtures now cover five detached supervisor windows,
including an interrupted artifact seal and a committed result before its CLI
hint. Store tests cover inbox-insert rollback and concurrent SQLite writer
contention. File-based results and optional OMP reports have bounded,
descriptor-checked reads; intermediate symlink escape is rejected. A failed
or interrupted separately launched Herdr-backed OMP worker is reported as
`lost` with unresolved effects, not falsely as a stopped one-shot process.
The unsigned cooperative-local platform boundary remains unchanged.

## 1.0.3 — 2026-09-14

Bind each owner to a Codex session epoch. Unbound or stale sessions can no
longer read, acknowledge, cancel, revise, or decide a task through the CLI.
`brgr bind TASK` explicitly transfers ownership without rewriting pending
results or decisions. Decision and acknowledgment transactions verify the
current epoch; a late old SessionStart hook cannot take it back. Existing
manual tasks require one explicit bind before follow-up. Hook inbox hints also
include pending items from owners transferred into the current session. The unsigned,
cooperative-local platform and optional best-effort Herdr cleanup are unchanged.

## 1.0.2 — 2026-09-14

Approved agents can activate an authored declarative process manifest after
bounded probes, contract tests, and an authorized scratch run. Admission is
durable before detached supervision; abandoned or pre-start-cancelled tasks
settle to one inbox result. Retries now require a durable pre-spawn failure
grant. The control home cannot overlap the source workspace, and artifact
imports verify the opened file identity. This remains an unsigned,
cooperative-local macOS arm64 release.

## 1.0.1 — 2026-09-14

Reject candidate results whose artifact is missing, forged, or fails sealed
byte verification before publishing them to an owner inbox. This closes a
store API path that could create a candidate the owner could not safely decide.
All other v1.0.0 feature and platform limits remain unchanged.

## 1.0.0 — 2026-09-14

First managed local release. Codex can start bounded fresh tasks through OMP,
GJC, Cursor CLI, and Command Code; brgr seals results, stores one durable owner
inbox item, and records explicit accept/reject decisions. Rejected candidates
can be corrected as immutable task revisions. A tested generic process recipe
supports future CLIs with a documented `--prompt-file` contract.

The macOS arm64 archive is unsigned and not notarized. See
[release notes](docs/releases/v1.0.0.md) and the
[distribution guide](docs/guides/unsigned-distribution.md) for supported behavior and
limitations.
