# brgr v2.0.0

brgr is now installable as a Herdr plugin on macOS with Herdr 0.9 or newer.
Use **Open brgr status** for a live, read-only task board, **Open Codex for
brgr** to start Codex in the selected workspace or linked worktree, and
**Check brgr** to inspect the local integration and harnesses. Source installs
build the locked Rust binary.

The v1 managed-run contract remains intact: bounded execution, sealed result,
durable owner inbox, and a final accept/reject decision made by Codex. Herdr
pane state or action completion never accepts a task. Existing CLI commands,
stored results, and decisions remain usable without Herdr or a store migration.

Install after the tag is available:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.0.0
```

The separate unsigned Apple Silicon CLI archive remains available. Plugin
installation needs a local Rust toolchain for its source build. Windows,
Linux, notarized distribution, and atomic external pane cleanup are outside
this release's verified surface. The prior public v1 readiness checklist was
not retroactively marked complete.

See [the v2 plugin contract](https://github.com/justn-hyeok/brgr/blob/v2.0.0/docs/v2-herdr-plugin.md)
and [unsigned distribution guide](https://github.com/justn-hyeok/brgr/blob/v2.0.0/docs/unsigned-distribution.md).
