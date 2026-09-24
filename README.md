# brgr

`brgr` is a harness-neutral local supervisor for bounded agent runs. It turns a
fresh process attempt into a sealed artifact, a durable owner inbox item, and
an explicit accept or reject decision. Herdr can present an OMP session, but it
does not own task identity or completion.

## Status

v2 packages brgr as a Herdr 0.9+ plugin for Apple Silicon macOS 15 or newer.
Herdr hosts a read-only task board and a Codex pane; brgr still owns task
identity, sealed results, and the durable inbox, while Codex alone decides
accept or reject. The v1 CLI and stored results remain usable without Herdr.
Release archives are
unsigned and not notarized. Read [the unsigned distribution guide](docs/unsigned-distribution.md)
before sharing or running a downloaded binary.

The v1 managed-run contract is retained. The v2.1.0 release closed the
[v1 readiness checklist](docs/v1-readiness-checklist-2026-09-14.md) under its
recorded personal-use scope and evidence limits. v2.2.0 adds recursive worker
delegation and an attempt-scoped message mailbox. Its verified scope and open
limits are described below.

## Herdr plugin

Install the current public v2 plugin from GitHub:

```bash
herdr plugin install justn-hyeok/brgr --ref v2.2.2
```

The plugin builds `brgr` from the pinned Cargo lockfile, so a Rust toolchain is
required for installation. For local development, build with `cargo build
--release --locked -p brgr-cli` and run `herdr plugin link .` from this checkout.
The manifest is [herdr-plugin.toml](herdr-plugin.toml).

Herdr exposes three workspace actions: **Open brgr status** opens a live,
read-only board of the latest 20 tasks; **Open Codex for brgr** starts a new
Codex pane in the selected worktree or workspace; **Check brgr** reports
integration and harness health. Opening the Codex pane installs or updates only
brgr-owned Codex hooks and its skill, then launches Codex with the plugin's
built binary on `PATH`. A private bridge tied to that pane executes brgr CLI
commands in the Herdr host, where supervisor process inspection is available;
Codex keeps its ordinary command sandbox and receives access only to an
ephemeral bridge directory. The host accepts managed task, result, decision,
health, and cleanup commands, pins them to the selected workspace and brgr
control home, and rejects harness mutation or integration commands. Plugin
installation itself does not run a paid model or activate a harness. Register
an approved executable with a small authorized scratch run outside the plugin
Codex pane before using it for a task. The board shows task status, route, and
decision state without revealing prompts or artifact contents.

See [the v2 plugin contract](docs/v2-herdr-plugin.md) for ownership, context,
recovery, and verification boundaries.

GJC, Cursor CLI, Command Code, Devin CLI, and OMP have bounded one-shot process recipes.
Herdr is optional:
the separate `local.omp-herdr` adapter uses `omp-role` when an interactive pane
is explicitly wanted. After Codex accepts or rejects that adapter's result,
brgr closes only its recorded pane; use `--keep-pane` to retain it. See the
cleanup safety limit below.

| Route or operation | v1 status |
|---|---|
| OMP process, GJC, Cursor CLI, Command Code, Devin CLI | Certified bounded fresh-run contract; exact model/effort only where the activated recipe supports it |
| Approved unfamiliar one-shot CLI | Supported after manifest contract test and authorized scratch activation |
| `local.omp-herdr` presentation | Optional best effort; external worker stop is not certified and uncertain failure is `lost` |
| Resume, in-flight steering, provider-side undo | Deferred; unsupported requests must fail rather than silently fall back |

`local.devin` runs `devin` in documented prompt-file print mode with smart
permissions and the non-interactive workspace-trust override. It uses the model
already selected in Devin CLI. Run `/model` once in an interactive Devin session
if that configured default is stale. Devin's current JSON catalog exceeds
brgr's bounded probe limit, so `brgr run --model` and `--effort` intentionally
fail for this route instead of guessing a family variant. See the
[v2.0.2 live receipt](docs/live-devin-cli-v2.0.2-2026-09-15.md).

For JSONL OMP process results, brgr checks every assistant event's native
`provider/model` against an explicitly requested selector before publishing a
candidate. `brgr result TASK` exposes that model in a separately committed
`route_observation` receipt; the sealed v1 result JSON and its decision digest
remain unchanged for older binaries. OMP does not expose a
verified effort value in this event stream, so `effort_source` remains
`unavailable`; other process recipes likewise report unavailable native route
identity unless their adapter supplies evidence. A missing or different OMP
model fails closed rather than becoming an accepted candidate.

For lowest available Luna effort on the installed CLIs tested on 2026-09-14:
OMP uses `openai-codex/gpt-5.6-luna` with `--effort low`, GJC uses the same
model with `--effort minimal`, Cursor CLI uses the exact model variant
`gpt-5.6-luna-none` without an effort flag, and Command Code uses
`gpt-5.6-luna` with `--effort low`. Recheck each executable's native catalog
after upgrading it. These are route requests; only OMP and GJC expose a
native model observation, and none expose a verified effort observation.

## Standalone Codex and CLI

### Recursive workers

v2.2.1 can open the brgr-owned `worker` plugin pane for a detached run from
any Herdr pane. Run `brgr config set-auto-worker-pane true` once to enable
automatic panes for ordinary Herdr callers. The brgr plugin Codex pane opens
worker panes without this setting. Set `brgr config set-worker-placement tab` to open future workers in
new tabs; `adjacent` restores the default. The setting is stored at
`BRGR_HOME/config.toml` and never moves panes already running.

A worker receives `$BRGR_BIN`, its exact task/attempt identity, and a scoped
owner session. It may call `brgr run` again to create a child; brgr records the
parent edge and delivers the child result to that worker's inbox. The parent
must read and accept/reject a candidate (or acknowledge another outcome)
before its own successful result can become a candidate. `brgr wait TASK`
waits for a child result without approving it. `brgr message send|list|wait|ack`
lets each side ask and answer questions on the active attempt; unanswered
questions block a successful candidate. Standalone runs opt in with
`--enable-delegation`; plugin workers enable it automatically. See
[recursive worker contract](docs/recursive-workers.md) for the verified scope
and remaining gates, including the
[live OMP → GJC → GJC receipt](docs/live-recursive-bridge-2026-09-23.md) and
[bidirectional message receipt](docs/live-bidirectional-bridge-2026-09-23.md).
Earlier v2.1.0 binaries do not contain these commands.

Install the local binary and Codex integration, then start a new Codex session:

```bash
cargo install --path crates/brgr-cli --locked --root "$HOME/.local"
brgr integrate codex install
```

Ask Codex, for example, “GJC를 작은 scratch 작업으로 검증해 등록하고,
이 변경을 검토하면서 실패 사례도 확인해줘.” Registration may invoke the
selected harness's model, so authorize its scratch prompt explicitly.
Codex owns the task criteria and the accept/reject decision; brgr owns the
bounded execution, sealed bytes, and durable inbox. The CLI remains an escape
hatch for inspection and explicit operations:

```bash
brgr run "Review this change" --harness local.gjc --criterion "findings cite changed lines"
brgr status
brgr result TASK
brgr accept TASK --reason "criteria verified"
```

If a candidate misses the criteria, use `brgr reject TASK --reason "..."` and
`brgr revise TASK "Corrected request" --criterion "new observable check"`.
The old result and decision remain intact. `brgr result TASK --ack` acknowledges
a failed, cancelled, or lost result; acknowledgment is not acceptance. A clean
Git source gets a dedicated task worktree. Dirty changes are rejected unless
`--allow-clean-head-snapshot` explicitly excludes them.
Git is needed for Git-backed tasks; a non-Git workspace can run without Git.

Codex sessions bind their owner automatically at first task or SessionStart.
A bare CLI run without a session can finish but cannot expose, acknowledge, or
decide its result until `brgr bind TASK --session SESSION` explicitly claims it.
When the owning Codex session changes, run `brgr bind TASK` in the new session
before reading or deciding; the old session's epoch is then stale. For a CLI
fixture, set both `BRGR_OWNER_ID=codex:example` and
`BRGR_SESSION_ID=example-session` on run and follow-up commands.

`brgr integrate codex status|uninstall` checks or removes only brgr-owned
entries. For OMP, Cursor CLI, Command Code, Devin CLI, or an approved unfamiliar CLI, use
`brgr harness draft`, `brgr harness test`, then an authorized scratch run with
`brgr harness activate` and `brgr harness status`. Pass `--model MODEL` when
that exact manifest supports model selection. Do not infer support for flags
absent from the installed executable's help.
See [agent-authored manifests](docs/custom-harness-registration.md) for a
documented CLI whose prompt shape needs a custom declarative recipe.
For an already approved executable, `brgr harness add EXECUTABLE --workspace
SCRATCH --prompt "small authorized probe"` is the combined probe, contract-test,
scratch, activation, and health-check escape hatch. `--presentation-only`
registers the optional Herdr adapter without claiming a managed scratch run.
Process activations created before this scratch requirement remain on disk but
cannot start a new task; re-add each approved executable with an authorized
scratch workspace and prompt. Existing sealed results and decisions are kept.
An exact `--model` additionally requires an activation with a bounded native
model-catalog recipe. Older activations lacking it must be re-certified; an
unknown selector fails before a task worktree or model request is started.
Presentation-only Herdr model requests are checked against OMP's native
catalog before brgr admission and checked again by `omp-role` at dispatch.

`brgr doctor` probes every registered harness as well as the Codex integration
and store. It exits with `needs_attention` and names an unhealthy harness when
an executable or recipe has changed; re-add that harness with an authorized
scratch run before starting new work. JSONL process capture discards repeated
update events while retaining completed assistant/model evidence. Raw transport
remains bounded to 64 MiB, and the sealed final answer still obeys its manifest
artifact limit. Over-limit processes are stopped and produce a failed result.

`brgr cleanup status TASK` shows whether an owned OMP pane was closed or
retained. `brgr cleanup run TASK` retries a pending close. Neither command
removes a task worktree.

Herdr 0.9.0 accepts `pane.close(pane_id)` without conditional identity fields.
Brgr rechecks owner decision, inbox acknowledgment, pane ID, terminal ID,
immutable agent session, idle state, and protected-tab status immediately
before closing. Another actor could still change the pane between that check
and Herdr's close call. This is a best-effort cooperative-local guarantee, not
an atomic compare-and-close guarantee; use `--keep-pane` for shared sessions.

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -p brgr-cli -- --help
```

The local store defaults to `~/Library/Application Support/brgr`. Harness
output, help text, manifests, and reports are treated as untrusted data.
Before upgrading, retain a recoverable copy of the private registry and store.
An older `v1.0.5` binary cannot load a newly activated manifest containing
`model_catalog`; this fails closed without deleting results. Restore a matching
registry snapshot only after stopping new admissions and confirming no active
task depends on the newer activation. Never overwrite a newer live store with
an old snapshot.

See [Architecture](docs/architecture.md) for the managed-run contract.
