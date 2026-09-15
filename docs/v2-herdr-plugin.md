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
| `board` pane | Refreshes the latest 20 task states and decisions every two seconds; never returns prompt or artifact bytes. |
| `codex` pane | Installs only brgr-owned Codex hooks/skill, places the plugin binary on `PATH`, starts a fresh Codex session, and hosts its private brgr command bridge. |

Herdr supplies the workspace and worktree context. The Codex pane chooses the
linked worktree checkout first, then the workspace cwd, then the focused pane
cwd; a missing, relative, or nonexistent directory fails before launching an
agent. Each action pins that directory when opening its pane, so focusing the
status board cannot redirect a later Codex action into the plugin checkout.
It clears inherited Codex/brgr session variables so the new Codex
session establishes its own owner binding. All commands use argv arrays
without a shell. The `board` is read-only task metadata for the same OS user;
only the bound Codex owner can read sealed result contents or decide them.

Codex keeps its normal command sandbox. Its plugin pane adds only an ephemeral
bridge directory as a writable root and passes brgr CLI calls through the
private file bridge served by that pane's host process. The host pins every
request to the selected brgr control home and permits managed task, result,
decision, health, and cleanup operations. It rejects harness mutation, Codex
integration mutation, internal plugin entrypoints, and task workspaces outside
the selected Herdr workspace. Register or re-certify harnesses and change Codex
integration explicitly outside the plugin Codex pane. Requests retain the
requesting Codex session identity, a finite deadline, and bounded request,
process-output, and encoded-response sizes. Foreground budgets that exceed the
bridge's seven-day command limit (including its completion margin) fail before
admission. The bridge disappears when the Codex pane exits. A missing bridge
fails visibly; it does not trigger a generic sandbox bypass. This is a
cooperative same-user boundary, not protection from a hostile local process
with the user's permissions.

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

On 2026-09-15, Herdr 0.9.0 linked the plugin and a real Codex 0.154.0 pane
ran the cost-free `local.gjc` fixture through the host bridge. Task
`df628b4a-c9a1-4ef9-8cbb-eb3bf84b5fd0` produced a sealed 15-byte artifact
whose text was exactly `BRGR_FIXTURE_OK` and whose SHA-256 was
`770fc6713b7be966375c363b51c1fe2ccab11612c89fb9e987089fd013f57504`.
Codex checked those bytes and recorded decision
`ca873c24-cfba-404d-8bc3-adc093dbea7e` as `accepted` against the persisted
result digest. The owner inbox was acknowledged, and the board showed
`accepted` separately. This is a real Codex flow over a fixture harness, not
a paid GJC/OMP run or human observation.

Before the bridge, direct `brgr` execution inside Codex's macOS command
sandbox could not inspect the supervisor process (`Operation not permitted`)
and correctly produced `lost` results without acceptance. The host bridge
addresses that observed failure. An installed v1.0.9 binary also produced a
sealed fixture candidate in an isolated store; v2.0.0 read the unchanged
`brgr/v1` result and recorded an explicit owner decision. A separate isolated
install upgraded v1 Codex hooks and skill to v2 using its ownership receipt.

A clean Herdr GitHub source install of PR commit `c363109` built the plugin
binary and registered its actions and panes. Codex 0.154.0 launched from that
managed checkout with a private bridge directory at mode `0700`. It produced
fixture task `6e0da337-ce9a-4dd6-aa43-c2cc52e6ce23`; the stored artifact
again matched `BRGR_FIXTURE_OK`. Codex recorded decision
`917e354a-2b99-440c-906b-2cf9f962cad5` as `accepted`, and an independent
store read confirmed the result digest, owner session, acknowledged inbox, and
board state. The final tagged GitHub package and post-merge release artifacts
remain to be verified at their exact commits.
