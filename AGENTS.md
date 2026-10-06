# Repository Guidelines

## Project Structure & Module Organization

`brgr` is a Rust workspace for harness-neutral local agent orchestration. CLI, Codex hooks, and OMP/Herdr integration live in `crates/brgr-cli/`. Task state and supervision live in `crates/brgr-core/`; versioned wire types in `crates/brgr-protocol/`; SQLite and sealed artifacts in `crates/brgr-store/`; shell-free execution in `crates/brgr-runner/`.

Harness registration belongs in `crates/brgr-registry/`; its generic process recipe must not require a core switch statement. Reusable fixtures are in `testdata/fixtures/`, wire schemas in `schemas/`, and operator guidance in `docs/`.

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

The history uses Conventional Commit subjects such as `feat(store): seal result artifacts` and `fix(cli): preserve Codex hooks`. Keep commits narrowly scoped. Pull requests must explain the contract affected, include test evidence, identify migration or compatibility risk, and confirm that no harness/model fallback or acceptance decision occurs implicitly.

Batch releases. Collect fixes on `main` and cut one release for a coherent set of changes, not one per fix; a same-day follow-up release is only for a release that is broken (failed build, wrong binary, data loss). Docs-only changes ship without a release.

## Releasing

1. On a branch, bump the version in `Cargo.toml` (`[workspace.package]` and the five internal crates in `[workspace.dependencies]`) and `herdr-plugin.toml`, and the `--ref`/`--tag` in `README.md`. Add a `CHANGELOG.md` entry, write the release notes in `docs/releases/`, named after the tag, and link them first in the `docs/README.md` index. Run `cargo update --workspace --offline`.
2. Open a `release: vX.Y.Z` pull request, wait for CI, and squash-merge it.
3. On the merged `main`, run `git tag -a vX.Y.Z -m "brgr vX.Y.Z"` and push the tag.

The tag runs `.github/workflows/release.yml`. It tests and builds the binary, publishes the GitHub release as Latest with those notes, then publishes all six crates to crates.io with the `CARGO_REGISTRY_TOKEN` secret, dependencies first. If a run fails part way, for example on a crates.io 429, rerun it: crates already at that version are skipped and an existing release is updated. A release is done when the GitHub release is Latest and `cargo install --locked brgr-cli` installs the new version.

## Operating Contract

Choosing brgr to orchestrate work means the owner trusts the workers it launches. These rules govern what brgr does to those workers; an agent operating someone's pane by hand still follows the parent workspace policy on approvals and questions. Do not raise objections against a worker's tool permissions in reviews. Owner decisions are not affected: the owner still decides `accept` or `reject` on every sealed result, and brgr never decides one implicitly.

1. **Workers always run with full permissions.** A worker is launched with its harness's full-permission mode (`--yolo`, `--dangerously-skip-permissions`, `bypassPermissions`, and equivalents), and nothing lowers it: `--permission` and the `permission` config key are accepted for old scripts and ignored with a note, a stored task's level is ignored when it runs, and the permission cap commands (`config set-max-permission`, `clear-max-permission`) no longer exist. brgr adds no cap, downgrade or flag denylist to a worker launch; only the model-catalog probe, which runs unattended with no task, keeps its guard. The `permission` field stays in the schema and in config files so stored data still loads.
2. **Screens that block a worker are answered, not left for a person.** `crates/brgr-cli/src/pane_adapter/screens.rs` holds the rule table, each rule pinned by a test on a captured or labelled-synthetic screen. It answers: the folder-trust prompt for the task's own workspace (Claude, Codex); a "press Enter to continue" notice; Claude Code's Bypass Permissions warning and a newly found MCP server (both accepted; their tests use screens written from the documented dialogs, not captured panes); an update offer, by choosing its skip option ("Skip", "Not now", "Later"), never "Update now"; and a self-update that has already been applied, with Esc. Nothing is pressed while Herdr reports `working`, and an update menu is checked before any "press Enter" line so Enter never lands on "Update now". Every other screen is reported to the owner and fails the run after three minutes with the screen's text; extend the table when one appears. Do not blind-press Enter on a menu brgr has not recognized. OpenCode and Claude Code are launched with self-update disabled (`self_update_env` in `pane_adapter/native.rs`); other harnesses rely on the update-offer rule. Each key brgr presses to answer a screen is appended to `runs/<task>-r<rev>.screens.log` (submitting a prompt is not logged).
3. **Messages land in both directions.** A worker's question reaches the owner, and an owner's reply or follow-up reaches the worker, without either side polling. Delivery uses a live pane prompt when the recipient pane is idle and verified, hook injection at the next Codex turn, and the pending state shown by `brgr status TASK --tree`; it does not depend on one idle instant. Claude Code owners (`claude:<CLAUDE_CODE_SESSION_ID>`) are identified by process ancestry. Codex owners are identified by what the panes show (`crates/brgr-cli/src/caller_pane.rs`): the one Codex pane whose screen shows `Ran brgr --as <session>`; Herdr's recorded session for a pane is never trusted, because Codex's shared daemon makes Herdr's hook report the wrong pane. When no pane or several panes show the call, nothing is guessed.

A failed or lost run is not left for the owner to discover. The completion push for it is a `brgr_failure` notice with the outcome and reason, and every brgr command a bound owner runs ends with a stderr note listing up to three unacknowledged failed or lost runs (`crates/brgr-cli/src/failure_banner.rs`), repeated until `brgr result TASK --ack`. The note reaches an owner that runs brgr commands from a bound session; an owner with no session binding gets only the push.

## Error Memo

brgr folds every failed or lost run it settles (supervisor result, recovery after a crashed supervisor, and runs that never started) into `$BRGR_HOME/error-ledger.json` and writes `$BRGR_HOME/brgr_error_issue_memo.md` (`crates/brgr-cli/src/error_memo.rs`). One entry per fingerprint of the error class, with ids, panes, paths, counts and any quoted pane text removed. `brgr errors` lists it and `brgr errors preview` prints the exact issue text without sending it. With `brgr config set-issue-reporting OWNER/NAME` (off by default), a background `brgr errors file` files one issue per fingerprint once it has been seen `--min-count` times (default 2), deduplicates by a `brgr-fp:` marker in the issue body, and comments only at 5, 25 and 100 sightings. Issue text holds the error class, a redacted sample cut at the same points, and counts; never an objective, prompt, report, file content or quoted screen. Keep it that way. Turning reporting on is blocked through the Herdr host bridge.

## Working Rules for Bugs

Reproduce first with a failing test or command; if it does not reproduce, say so rather than reporting a fix. Add a regression test with each fix and confirm it fails when the fix is reverted. One bug per change. Report what was verified and what was not, for example "unit tests pass" separately from "checked in a live Herdr pane".

## Security & Architecture Boundaries

Treat harness output, manifests, and help text as untrusted data. Execute argv arrays without a shell, allowlist environment variables, bound output sizes and deadlines, and keep the supervisor store outside worker-writable worktrees. Herdr is an optional presentation adapter, never task identity or the completion oracle. The Operating Contract governs what workers may do; these boundaries govern how brgr parses and stores what workers produce.
