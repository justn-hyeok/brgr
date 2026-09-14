# brgr

`brgr` is a harness-neutral local supervisor for bounded agent runs. It turns a
fresh process attempt into a sealed artifact, a durable owner inbox item, and
an explicit accept or reject decision. Herdr can present an OMP session, but it
does not own task identity or completion.

## Status

The v1 target is Apple Silicon on macOS 15 or newer. Release archives are
unsigned and not notarized. Read [the unsigned distribution guide](docs/unsigned-distribution.md)
before sharing or running a downloaded binary.

GJC, Cursor CLI, Command Code, and OMP have bounded one-shot process recipes.
Herdr is optional:
the separate `local.omp-herdr` adapter uses `omp-role` when an interactive pane
is explicitly wanted. After Codex accepts or rejects that adapter's result,
brgr closes only its recorded pane; use `--keep-pane` to retain it. See the
cleanup safety limit below.

## Start from Codex

Install the local binary and Codex integration, then start a new Codex session:

```bash
cargo install --path crates/brgr-cli --locked --root "$HOME/.local"
brgr integrate codex install
brgr harness add "$(command -v gjc)"
```

Ask Codex, for example, “GJC로 이 변경을 검토하고 실패 사례도 확인해줘.”
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

`brgr integrate codex status|uninstall` checks or removes only brgr-owned
entries. For OMP, Cursor CLI, Command Code, or an approved unfamiliar CLI, use
`brgr harness draft`, `brgr harness test`, then an authorized scratch run with
`brgr harness activate` and `brgr harness status`. Pass `--model MODEL` when
that exact manifest supports model selection. Do not infer support for flags
absent from the installed executable's help.
See [agent-authored manifests](docs/custom-harness-registration.md) for a
documented CLI whose prompt shape needs a custom declarative recipe.

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

See [Architecture](docs/architecture.md) for the managed-run contract.
