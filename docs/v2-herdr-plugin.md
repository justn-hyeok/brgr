# brgr v2 Herdr plugin contract

The public v2 package is `herdr-plugin.toml` plus the Rust `brgr` binary built
from this repository. Herdr is the workspace and pane host. The v1 managed-run
path remains available through the same binary: fresh bounded execution,
sealed artifact, durable owner inbox, and explicit Codex accept/reject. Herdr
pane state, plugin action completion, and terminal exit do not become task
identity or the completion oracle.

## Entrypoints

| Herdr action or pane | Behavior |
| --- | --- |
| `brgr.open` | Opens a `board` tab in the selected Herdr workspace. |
| `brgr.codex` | Opens a Codex tab for that workspace's linked worktree or cwd. |
| `brgr.doctor` | Checks the brgr store, registered harnesses, and Codex integration. |
| `board` pane | Refreshes the latest 20 task states and decisions every two seconds; never reads prompt or artifact bytes. |
| `codex` pane | Installs only brgr-owned Codex hooks/skill, places the plugin binary on `PATH`, and starts a fresh Codex session. |

Herdr supplies the workspace and worktree context. The Codex pane chooses the
linked worktree checkout first, then the workspace cwd, then the focused pane
cwd; a missing, relative, or nonexistent directory fails before launching an
agent. Each action pins that directory when opening its pane, so focusing the
status board cannot redirect a later Codex action into the plugin checkout.
It clears inherited Codex/brgr session variables so the new Codex
session establishes its own owner binding. All commands use argv arrays
without a shell. The `board` is read-only task metadata for the same OS user;
only the bound Codex owner can read sealed result contents or decide them.

## Installation and migration

`herdr plugin install justn-hyeok/brgr --ref v2.0.0` builds from `Cargo.lock`.
`herdr plugin link .` links a local built checkout for development and does not
run build commands. Plugin installation does not register a harness, perform a
paid scratch run, erase the v1 store, or close any pane/worktree. Opening the
Codex pane updates only brgr-owned Codex integration entries. Already sealed
v1 results and decisions remain in the same brgr store; there is no wire or
store schema migration. Existing `brgr run|status|result|cancel|harness` commands
remain the standalone escape hatch without Herdr.

The plugin requires Herdr 0.9 or newer on macOS. The optional legacy
`local.omp-herdr` adapter still has best-effort pane cleanup and cannot prove
external worker cancellation. It is separate from the v2 status and Codex
panes. Windows, Linux, notarized macOS distribution, and automatic recovery of
unknown executable drift are not certified by this plugin package.

## Verification gates

- The installed Herdr binary links and lists the plugin manifest, actions, and
  panes without warnings or an ID collision.
- A plugin action opens the status tab in a non-focused isolated Herdr session;
  the board shows candidate then accepted only after a separate Codex owner
  decision. It never prints the task objective or sealed artifact text.
- The Codex pane receives the selected worktree/cwd, runs with a fresh session,
  and has the plugin-built `brgr` on `PATH` with matching owned hooks and skill.
- A real Codex request through that pane reaches a sealed result, owner inbox,
  source-grounded explicit decision, and a board update without asking the user
  to copy a task ID. The v1 CLI contract tests still pass.
- An absent Herdr host, invalid context, changed harness, failed launch, or
  unsupported model/effort fails visibly without silently switching routes or
  accepting a candidate.

The first three gates have isolated local fixture evidence on Herdr 0.9.0.
An installed v1.0.9 binary produced a sealed fixture candidate in an isolated
store; v2.0.0 read the unchanged `brgr/v1` result, recorded an explicit owner
decision, and showed that decision on the board. A separate isolated install
upgraded the v1 Codex hooks and skill to v2 using its ownership receipt.
These fixture checks do not count as a real Codex-in-plugin request.
The real Codex-in-plugin request and public v2 package remain unverified until
their respective receipts are recorded.
