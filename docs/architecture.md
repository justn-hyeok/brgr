# Architecture

`brgr` has one minimum managed contract:

```text
bounded fresh run -> sealed result -> durable inbox -> owner decision
```

The protocol crate defines immutable task revisions, attempt identity, terminal
outcomes, artifact references, and decisions. The core crate validates state
transitions. The runner executes declarative argv recipes without a shell. The
store seals artifact bytes before committing result and inbox metadata. The
registry activates only recipes supported by bounded help/version evidence.

Execution and presentation are separate. GJC, Cursor CLI, and Command Code use
one-shot process recipes. The current OMP adapter requires Herdr for launch and
lifecycle evidence; Herdr is not a requirement of the core contract, and
pane and session identifiers remain metadata. A missing pane must not erase an
already sealed result.

Candidate output is never accepted because a process exits successfully. Codex
checks the task's acceptance criteria and records an accept or reject decision
against the exact result digest. Transport acknowledgment only marks an inbox
item as received.

The v1 trust model assumes installed same-user harnesses are cooperative. File
permissions, digest locks, size limits, typed events, and environment allowlists
prevent accidents and stale identity reuse; they do not isolate a hostile
same-user process.

Each attempt records a launch intent and supervisor incarnation before the
harness starts. A restarted observer compares the recorded identity with the
live process. An uncertain run becomes `lost` with unresolved effects and is
never retried automatically. Only a transient failure before process spawn may
use the one remaining attempt in the two-attempt budget.

Brgr records its OMP pane and terminal IDs, immutable agent session, task and
attempt IDs, and parent pane. After the owner decision and inbox acknowledgment,
it rechecks those fields plus idle state and protected-tab status, then calls
Herdr's official `pane.close(pane_id)` route. It targets only panes with a
brgr launcher receipt and does not close a working, blocked, mismatched, or
`--keep-pane` pane. It never deletes the worktree. A failed close remains
pending for a bounded retry; shared use should select `--keep-pane`.

Herdr 0.9.0 has no atomic conditional close. The fresh check and close are
separate operations, so another actor changing the pane in that small
interval remains a documented race. Brgr does not claim adversarial or
atomic safety for pane cleanup.
