# Recursive worker bridge (development)

The intended topology is `Codex ↔ OMP ↔ GJC ↔ GJC`, with the same brgr
delegation contract at every edge. A child result belongs to the exact parent
attempt that created it. A child can become a parent without a pair-specific
OMP-to-GJC or GJC-to-GJC adapter.

## Implemented in this branch

- `brgr run --enable-delegation` gives a one-shot worker `$BRGR_BIN`,
  `BRGR_HOME`, its parent task/attempt IDs, and an owner session. A worker's
  `brgr run` creates a durable parent edge; nesting is capped at eight edges.
- `brgr wait TASK --timeout-seconds N` observes a terminal result. `result`,
  `accept`, `reject`, and acknowledgment retain their existing exact-owner and
  sealed-result checks. A parent that exits with unsettled children gets a
  failed result rather than an accepted candidate.
- `brgr message send|list|wait|ack` exchanges bounded, durable messages on the
  exact active task attempt. Either side can ask a question or reply. A reply
  must reference an opposite-direction question on that attempt. Unanswered
  questions prevent the worker's successful exit from becoming a candidate;
  an unacknowledged reply remains available to its recipient after exit.
- With `herdr.auto_worker_pane = true`, detached runs from ordinary Herdr
  panes open a manifest-declared `worker` pane. The brgr plugin Codex pane
  always opens one. Placement is `adjacent` by default, splitting the exact caller pane
  without focus. A
  `tab` preference keeps the worker visible in a new tab. The selection is
  stored in `BRGR_HOME/config.toml` and captured in the launch receipt:

  ```toml
  [herdr]
  worker_placement = "adjacent"
  ```

  Use `brgr config show` and `brgr config set-worker-placement adjacent|tab`.

The deterministic CLI fixture exercises `OMP → GJC → GJC` routing and
`GJC → GJC → GJC` recursion, each child's explicit decision, and the failure
path when a parent leaves a child unsettled. The same chain passes from a clean
Git repository with sibling task worktrees and from a relative control-home
path. A crashed detached supervisor
is reconciled to `lost` while its owner waits. A fake Herdr executable checks
both placement requests, and a worker-pane fixture reaches a sealed result and
owner decision. SQLite read-then-write paths use immediate transactions so
concurrent worker admission cannot promote a stale snapshot into a write.

The [2026-09-23 live receipt](live-recursive-bridge-2026-09-23.md) records a
real Herdr 0.9.0 OMP → GJC → GJC chain: adjacent worker panes, two parent
decisions, a final Codex-owner decision, and a separate `tab` placement run.
The [bidirectional receipt](live-bidirectional-bridge-2026-09-23.md) records
live question, reply, and acknowledgment traffic in both directions at each
edge. Its accepted rerun completed a single Codex ↔ OMP ↔ GJC ↔ GJC chain
with explicit decisions at all three levels. The rerun used longer parent
deadlines after an earlier middle GJC timed out.
An [ordinary Herdr pane receipt](live-ordinary-herdr-pane-2026-09-23.md)
records the v2.2.1 caller path using a deterministic fixture.

## Remaining product gates

- The worker pane currently runs brgr's bounded process supervisor. It is not
  an interactive GJC TUI. Herdr detected the underlying GJC process as working
  in the live test, but a follow-up-capable GJC session has not been proven.
- The message path is a cooperative CLI mailbox, not a live interactive GJC
  TUI follow-up channel. Integrate pane custody and conservative cleanup for
  new worker panes.
- Improve placement for deep chains: the live four-pane tab narrowed the final
  two panes to 15 columns each.
- Confirm behavior for dirty parent worktrees, lost worker panes, cancellation,
  restart recovery, and concurrent admissions across a nested chain. The
  current fixture covers clean Git worktrees and existing crash-window tests;
  live nested Herdr/model execution is covered only for the recorded runs.
- The v2.3.0 completion loop adds selected-file dirty snapshots, explicit
  capability admission, sealed evidence, separate conflict-checked integration,
  a bounded subtree cancel, and exact-session Herdr completion callbacks.
  See [completion loop plan](completion-loop-plan.md). Fake-Herdr fixtures
  cover callback retry and recipient identity; a fresh real Codex pane wake
  has not yet been run for this branch.

Herdr owns terminal layout and observed pane state. brgr owns task identity,
attempts, sealed artifacts, inboxes, and decisions. Neither a pane returning
to idle nor a process exiting successfully is an acceptance decision.
