# brgr v2.0.1

This patch hardens the personal-use v2 path across the Herdr host bridge, Git
worktree admission, harness health checks, SQLite contention, and the read-only
task board.

The plugin Codex pane now exposes only an ephemeral bridge directory. The host
accepts managed task, result, decision, health, and cleanup operations pinned
to the selected workspace and brgr control home, while rejecting harness or
integration mutation. Bridge timeouts, output overflow, and pane shutdown stop
the full spawned process group. Requests, process output, and encoded responses
are bounded.

Concurrent task admissions share one repository lock across linked worktrees,
preserve existing paths and branches, and release the lock before foreground
execution. Harness health now distinguishes executable changes, spawn failure,
timeout, nonzero exit, and probe-evidence drift. Codex integration checks the
recorded hook binary without treating an equivalent plugin binary at another
path as drift.

The Herdr board opens the existing SQLite store read-only, avoids schema or
artifact initialization, projects only the fields it displays, validates task,
result, and decision identities, and sanitizes terminal control characters.
No task wire format, store migration, implicit harness/model fallback, or
automatic accept/reject behavior changes.

Install the plugin with:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.0.1
```

The downloadable CLI archive is unsigned and not notarized, targets Apple
Silicon macOS 15 or newer, and includes checksums and an SPDX SBOM. Plugin
source installation requires a local Rust toolchain. This release does not
declare the broader public v1 checklist complete.
