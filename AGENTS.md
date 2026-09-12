# Repository Guidelines

## Project Structure & Module Organization

`brgr` is a Rust workspace for harness-neutral local agent orchestration. Keep executable wiring in `crates/brgr-cli/` and shared task, attempt, result, and acceptance rules in `crates/brgr-core/`. Versioned wire types belong in `crates/brgr-protocol/`; persistence and artifact sealing belong in `crates/brgr-store/`; subprocess execution belongs in `crates/brgr-runner/`.

Place integrations behind explicit boundaries: generic recipes in `crates/brgr-adapter-process/`, OMP support in `crates/brgr-adapter-omp/`, and optional Herdr presentation in `crates/brgr-presentation-herdr/`. Store declarative harness packages under `harnesses/<harness>/` and reusable fixtures under `testdata/`. A harness addition must not require a switch statement in the core.

## Build, Test, and Development Commands

- `cargo build --workspace`: compile every crate.
- `cargo run -p brgr-cli -- --help`: exercise the user-facing entrypoint.
- `cargo test --workspace`: run unit, contract, and integration tests.
- `cargo test --workspace --all-features`: verify optional adapters together.
- `cargo fmt --all -- --check`: check canonical formatting.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: reject lint warnings.

Run commands from the repository root. Do not require Herdr for core or generic-process tests.

## Coding Style & Naming Conventions

Use stable Rust and standard `rustfmt` output with four-space indentation. Prefer small modules, explicit enums, and typed identifiers such as `TaskId` and `AttemptId`. Use `snake_case` for modules and functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Avoid `unsafe`, shell command strings, clever macros, and unnecessary generic or lifetime complexity. Expected domain failures should be typed errors; add context only at CLI or adapter boundaries.

## Testing Guidelines

Put unit tests beside the code and black-box tests in each crate's `tests/` directory. Name tests after observable behavior, for example `duplicate_terminal_event_keeps_one_inbox_item`. Cover state transitions, idempotency conflicts, process timeout/cancellation, artifact sealing, restart recovery, and malformed manifests. Live OMP or GJC checks are opt-in smoke tests; deterministic fixture executables remain the CI oracle.

## Commit & Pull Request Guidelines

No repository history exists yet. Use Conventional Commit subjects such as `feat(store): seal result artifacts`. Keep commits narrowly scoped. Pull requests must explain the contract affected, include test evidence, identify migration or compatibility risk, and confirm that no harness/model fallback or acceptance decision occurs implicitly.

## Security & Architecture Boundaries

Treat harness output, manifests, and help text as untrusted data. Execute argv arrays without a shell, allowlist environment variables, bound output sizes and deadlines, and keep the supervisor store outside worker-writable worktrees. Herdr is an optional presentation adapter, never task identity or the completion oracle.
