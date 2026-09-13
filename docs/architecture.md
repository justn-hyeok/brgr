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

Execution and presentation are separate. GJC uses a one-shot process recipe.
OMP may attach to Herdr for terminal presentation and lifecycle evidence, but
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

Brgr records owned OMP pane identity and queues cleanup after owner decision
and inbox acknowledgment. Herdr 0.9.0 exposes only `pane.close(pane_id)`, so
the queue reports eligibility but does not close panes automatically. This
avoids claiming an atomic identity check that Herdr cannot currently perform.
