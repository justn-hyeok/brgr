# brgr v2.2.0

brgr can delegate a bounded task from one registered process harness to
another, including OMP → GJC → GJC. Each child belongs to the exact parent
attempt. A parent settles child results with explicit decisions before its own
result can become a candidate. Workers can exchange durable, bounded
questions, replies, and acknowledgments with their owner in either direction.

The Herdr plugin opens a `worker` pane beside its caller by default. Set
`brgr config set-worker-placement tab` to use a separate tab. The setting is
stored in the brgr control home. Standalone CLI runs opt in to delegation with
`--enable-delegation`; plugin workers receive delegation context automatically.

An isolated live Herdr 0.9.0 run with real OMP and GJC processes completed one
Codex ↔ OMP ↔ GJC ↔ GJC chain with 12 acknowledged messages and three
explicitly accepted results. See the
[bidirectional receipt](live-bidirectional-bridge-2026-09-23.md) and
[recursive pane receipt](live-recursive-bridge-2026-09-23.md). The worker pane
hosts a bounded process, not an interactive GJC TUI. Deep adjacent splits may
be narrow; choose `tab` if needed. The live receipt does not cover arbitrary
dirty worktrees, lost panes, or every cancellation and restart interleaving.

Install the Herdr plugin on Apple Silicon macOS 15 or newer:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.2.0
```

The GitHub CLI archive is unsigned and not notarized. Plugin source installs
require a local Rust toolchain. Existing task and result records remain
available; this release adds durable delegation and message tables without
automatic acceptance or a harness/model fallback.
