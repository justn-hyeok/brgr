# brgr orchestration

brgr connects coding harnesses, task ownership, messages, sealed results, and
owned pane cleanup. The objective, scope, and user instructions determine the
work. brgr does not choose permission levels from task categories.

## Execution and configuration

The default is the registered executable's native TUI in a Herdr pane and
its declared full/YOLO arguments. `--headless` explicitly chooses a process
recipe. A missing TUI recipe, unsupported option, or unverified source produces
an error before admission. `--foreground` controls the supervisor; it does not
change this execution choice.

Calling options resolve from CLI flags, harness settings, global settings,
then product defaults. The launch records the resolved settings and source.
Model and effort remain native harness choices when omitted everywhere.

```sh
brgr config init
brgr config set harness local.claude-code
brgr config set effort medium --harness local.claude-code
brgr config set worker-placement tab
brgr config set deadline-seconds 1800
brgr config set argv '["--native-option","value"]' --harness HARNESS
brgr config show
brgr config check
```

`argv` is an array, never a shell command; named run options cannot be overridden
through it. `config check` probes registration and validates configured options
without executing a model. Existing explicit permission caps remain in effect.
Legacy `auto_worker_pane` and `prefer_print_mode` fields remain readable for
compatibility; execution shape is selected by `--headless` per call.

User instructions live at `$BRGR_HOME/BRGR.md`. On macOS the default is
`~/Library/Application Support/brgr/BRGR.md`. `config init` creates missing
examples, preserves existing files, and prints their actual paths. Instructions
are included with the task; their digest is recorded. A revision snapshots the
current instruction file again. Native harness config, authentication, and
session formats retain their existing owners.

Shared-daemon callers use `--owner-session SESSION --source-pane PANE` when
needed. The source must match a reported native session or independently
verified frontend process. Focus, cwd similarity, and newest-pane order are
not source identities. Codex hooks expose an exact calling context when found.

## Conversation

Parent and worker messages use `message send`, `list`, `wait`, and `ack`.
Replies identify the question with `--reply-to`. Native delivery records a
separate receipt; an acknowledgment records reading. Busy TUIs and native
dialogs keep ordinary messages queued. A confirmed unattempted send can retry;
an interrupted send remains observable rather than being pasted twice.

A worker is never left for a person to click. brgr answers the folder-trust
prompt for the task's own workspace (Claude, Codex), "press Enter to continue"
notices, update offers (by choosing the skip option) and an already applied
self-update (Esc), from the rule table in `pane_adapter/screens.rs`, and never
presses anything while the agent is working. Every key it presses is appended
to `runs/TASK-rN.screens.log`. OpenCode and Claude Code are launched with their
self-update disabled. A screen no rule covers is reported to the owner and
fails the run after three minutes with the screen's text; answer it earlier
with `input TASK --key KEY` or `--text TEXT`. A native dialog and a mailbox
question are separate inputs. The skill adds no user-approval loop and no
permission cap of its own: workers run with the harness's full-permission
option unless the user asks for a lower level.

Direct sibling communication requires an explicit **debate** group:

```sh
brgr debate start TASK_A TASK_B
brgr debate status GROUP
brgr debate send GROUP --to PEER --kind question --body TEXT
brgr debate send GROUP --to PEER --kind reply --reply-to MESSAGE --body TEXT
brgr debate list
brgr debate ack MESSAGE
brgr debate stop GROUP
```

Participants are sibling attempts with one owner. Worker identity comes from
the current managed attempt, not a substituted newer attempt. A stopped group
retains history and accepts no new conversation. Default tasks have no peer
delivery path.

## Completion, recovery, and cleanup

A final report is the completion signal. Idle alone does not end a conversation.
The worker records the report digest with the supplied `__seal-report` command;
this also ensures a session recovery server exists. The normal collector and
recovery use the original attempt and publish one sealed result/inbox item.
Incomplete reports wait; already recorded terminal results remain immutable.
Original cancellation and deadlines continue after collector loss.

`report TASK --body TEXT` publishes a worker answer without a worker source-file
write. Explicit read-only Claude/Cursor tasks can return marked final text
through a task-scoped native response hook. This follows the
[Claude Stop schema](https://code.claude.com/docs/en/hooks#stop) and
[Cursor response-hook schema](https://cursor.com/docs/hooks#afteragentresponse).
Global native hook configuration is preserved; temporary Cursor project hooks
are restored before result collection.

Inspect `result` and requested `diff` before an explicit decision. `accept`,
`reject`, and acknowledgment are separate from completion notification. Managed
report files are omitted from source diffs. Applying an accepted diff remains
a separate `apply --execute` operation.

After result handling, owned idle panes close unless `--keep-pane` was selected.
Pane/terminal and task/attempt identities are checked again before input and
close. Failed closes retain receipts and retry. After settlement, generated
reports move into the control home's archive so a clean task worktree can be
pruned; the sealed result remains available.
`status TASK` exposes session, settings, and source information.

## Verification

Deterministic fixtures cover defaults, explicit headless, trust input, busy
message queues, unclassified native editors, report recovery without a pane,
deadline retention, identity changes, close retry, and explicit peer groups.

The [2026-10-01 receipt](../evidence/orchestration-rework-2026-10-01.md) separates
local checks from live evidence. Native recipe declarations are not proof that
every configured provider currently has credentials, credits, or a working
model route.
