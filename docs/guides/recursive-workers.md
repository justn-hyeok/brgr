# Recursive worker bridge

GJC support has since been removed; the run records below are historical.

Current native TUI behavior is described in [orchestration](orchestration.md).
The dated receipts below preserve the earlier process-bridge evidence.

The intended topology is `Codex ↔ OMP ↔ GJC ↔ GJC`, with the same brgr
delegation contract at every edge. A child result belongs to the exact parent
attempt that created it. A child can become a parent without a pair-specific
OMP-to-GJC or GJC-to-GJC adapter.

## Delegation contract

- `brgr run --enable-delegation` gives a one-shot worker `$BRGR_BIN`,
  `BRGR_HOME`, its parent task/attempt IDs, and an owner session. Only a worker
  whose launch enabled delegation may create children; pane mode does not grant
  this permission implicitly. A worker `brgr run` is refused before admission
  with a pointer to `--enable-delegation` when the permission is absent.
  `--max-children` still caps concurrent children, and owner runs are unchanged.
  Nesting is capped at eight edges.
- Owner `brgr status` and `brgr status --brief` include every descendant of the
  owner's tasks, with its parent, harness, and requested model. An ancestor's
  owner can read descendant `status TASK` and `result TASK` after a worker exits.
  This grants read access only: child `accept` and `reject` remain with the
  worker that owns that child.
- `brgr cancel TASK` requests cancellation for the task and all live
  descendants. `--tree` remains accepted. The receipt lists all task IDs whose
  cancellation was requested; pane workers and their session processes are
  cleaned up as the attempts settle.
- `brgr wait TASK --timeout-seconds N` observes a terminal result. Ancestor
  owners can read `result TASK`; `accept`, `reject`, and acknowledgment retain
  their existing exact-owner and sealed-result checks. A parent that exits with an undecided or
  unacknowledged child result gets a failed result naming the child and the
  `result --ack`, `accept`, or `reject` command that settles it. A child
  cancelled by its parent is already settled.
- `brgr message send|list|wait|ack` exchanges bounded, durable messages on the
  exact active task attempt. Either side can ask a question or reply. A reply
  must reference an opposite-direction question on that attempt. Unanswered
  questions prevent the worker's successful exit from becoming a candidate;
  an unacknowledged reply remains available to its recipient after exit.
- Native TUI is the default. `--headless` explicitly selects the process recipe.
  brgr creates one owned native pane from the exact caller. Placement is
  configured as adjacent or tab and persists at `$BRGR_HOME/config.toml`.
  Messages queue while native input is busy; results are handled before cleanup.

The deterministic CLI fixture exercises `OMP → GJC → GJC` routing and
`GJC → GJC → GJC` recursion, each child's explicit decision, and the failure
path when a parent leaves a child unsettled. The same chain passes from a clean
Git repository with sibling task worktrees and from a relative control-home
path. A crashed detached supervisor
is reconciled to `lost` while its owner waits. A fake Herdr executable checks
both placement requests, and a worker-pane fixture reaches a sealed result and
owner decision. SQLite read-then-write paths use immediate transactions so
concurrent worker admission cannot promote a stale snapshot into a write.

The [2026-09-23 live receipt](../evidence/live-recursive-bridge-2026-09-23.md) records a
real Herdr 0.9.0 OMP → GJC → GJC chain: adjacent worker panes, two parent
decisions, a final Codex-owner decision, and a separate `tab` placement run.
The [bidirectional receipt](../evidence/live-bidirectional-bridge-2026-09-23.md) records
live question, reply, and acknowledgment traffic in both directions at each
edge. Its accepted rerun completed a single Codex ↔ OMP ↔ GJC ↔ GJC chain
with explicit decisions at all three levels. The rerun used longer parent
deadlines after an earlier middle GJC timed out.
An [ordinary Herdr pane receipt](../evidence/live-ordinary-herdr-pane-2026-09-23.md)
records the v2.2.1 caller path using a deterministic fixture.

## Current verification limits

The process-chain receipts above establish their recorded revisions. They do
not prove a mixed-harness native TUI chain for the current build. The current
[orchestration receipt](../evidence/orchestration-rework-2026-10-01.md) records
live Claude parent-message delivery, explicit sibling debate, read-only output,
and cleanup, plus provider-blocked GJC evidence. Fixture recovery and source
identity checks are separate from live provider availability.
