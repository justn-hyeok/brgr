# brgr

`brgr` is a harness-neutral local supervisor for bounded agent runs. It turns a
fresh process attempt into a sealed artifact, a durable owner inbox item, and
an explicit accept or reject decision. Herdr can present an OMP session, but it
does not own task identity or completion.

## Status

The v1 target is Apple Silicon on macOS 15 or newer. Release archives are
unsigned and not notarized. Read [the unsigned distribution guide](docs/unsigned-distribution.md)
before sharing or running a downloaded binary.

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

