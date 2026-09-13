# brgr

`brgr` is a harness-neutral local supervisor for bounded agent runs. It turns a
fresh process attempt into a sealed artifact, a durable owner inbox item, and
an explicit accept or reject decision. Herdr can present an OMP session, but it
does not own task identity or completion.

## Status

The v1 target is Apple Silicon on macOS 15 or newer. Release archives are
unsigned and not notarized. Read [the unsigned distribution guide](docs/unsigned-distribution.md)
before sharing or running a downloaded binary.

The current release candidate is still in verification. GJC supports bounded
fresh runs; OMP uses an installed `omp-role` and Herdr. Automatic pane closing
is disabled until Herdr offers identity-checked conditional close.

## Quick start

Build locally, register a harness, and start one task:

```bash
cargo install --path crates/brgr-cli --locked --root "$HOME/.local"
brgr harness add "$(command -v gjc)"
brgr run "Review this change" --harness local.gjc
brgr status
brgr result TASK
brgr accept TASK --reason "criteria verified"
```

Use `brgr reject TASK --reason "..."` if the sealed result does not meet the
task criteria. `brgr result TASK --ack` acknowledges a failed, cancelled, or
lost result; acknowledgment is not acceptance. The command uses a dedicated
Git worktree for a clean Git source. A dirty source is rejected unless you
explicitly pass `--allow-clean-head-snapshot`, which excludes those changes.

To make Codex the natural-language entry point, run `brgr integrate codex
install`, then start a new Codex session. It merges brgr hooks beside existing
hooks; `brgr integrate codex status|uninstall` checks or removes only brgr's
entries. Registration of an unknown CLI requires `brgr harness draft`,
`brgr harness test`, then an authorized scratch run with `brgr harness activate`.

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
