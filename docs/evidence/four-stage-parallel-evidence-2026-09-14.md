# Four-stage parallel verification — 2026-09-14

Scope: the existing fixture-backed `execution → sealed result → durable owner inbox → accept/reject` contract, **not** brgr v1 release or live-harness certification.

## Revision and method

- Worktree: `/Users/justn/dev/.worktrees/brgr-v1`, branch `loop/brgr-v1`, tracked HEAD `64119cbe56b0b955aadf59b3990780be00806cf0`. The [draft PR #1](https://github.com/justn-hyeok/brgr/pull/1) had this exact head; [CI run 34750502052](https://github.com/justn-hyeok/brgr/actions/runs/34750502052) was successful at the same SHA.
- Prepared binaries with `cargo test --workspace --all-features --locked --no-run` (exit 0). Four independent test-binary command groups were then launched concurrently. All four reported `START 2026-09-13T23:16:41Z`; all exited 0 within that UTC second. Rust test binaries were invoked directly after the locked Cargo build, avoiding Cargo build-lock serialization between groups.
- An initial trial with short names and `--exact` executed **0 tests** in three groups. It is discarded, not counted as a pass. The final run used the full names below, confirmed with each binary's `--list`.

## Parallel results

| Group | Actual test names (prefix `tests::` unless shown) | Observed result |
| --- | --- | --- |
| Execution, `target/debug/deps/brgr_runner-2d084b9c0092e7d9` | `executes_without_a_shell_and_captures_output`; `jsonl_result_keeps_only_final_assistant_text` | 2 passed, 0 failed |
| Seal, `target/debug/deps/brgr_store-b06858e6ae5250b4` | `artifact::tests::artifact_limit_rejects_oversized_input_without_publishing_it`; `artifact::tests::read_rejects_forged_path_modified_bytes_and_symlink`; `artifact::tests::same_content_reuses_one_private_content_addressed_file` | 3 passed, 0 failed |
| Inbox, store/core binaries | `duplicate_terminal_event_keeps_one_inbox_item`; `restart_reconciles_unknown_run_to_one_durable_lost_inbox_item` | 2 passed, 0 failed |
| Decision, `cli_contract`/store binaries | `real_cli_run_binds_candidate_to_its_owner`; `decision_and_ack_are_atomic_and_semantic_retries_are_idempotent`; `one_decision_is_bound_to_owner_and_result_digest`; `failed_ack_rolls_back_decision_insert` | 4 passed, 0 failed |

Total final parallel run: **11 passed, 0 failed, 0 ignored**. Each direct test-binary invocation used `--exact --nocapture` for its full test name; the seal group used filter `artifact::tests --nocapture` and ran three tests.

## Code-to-test anchors

| Contract | Code at audited HEAD | Test anchor |
| --- | --- | --- |
| Execute | `crates/brgr-cli/src/main.rs:300`, `crates/brgr-core/src/lib.rs:464`, `crates/brgr-runner/src/lib.rs:163` | `crates/brgr-runner/src/lib.rs:599`; full CLI fixture path at `crates/brgr-cli/tests/cli_contract.rs:81` |
| Seal | `crates/brgr-core/src/lib.rs:561`, `crates/brgr-store/src/artifact.rs:50` | `crates/brgr-store/src/artifact.rs:218`, `:235`, `:258` |
| Durable inbox | `crates/brgr-store/src/lib.rs:554` (result and inbox in one transaction), `:665` (owner pull), `crates/brgr-cli/src/main.rs:704` (hook) | `crates/brgr-store/src/lib.rs:1340`; recovery test at `crates/brgr-core/src/lib.rs:746` |
| Accept/reject | `crates/brgr-cli/src/main.rs:259`, `:568`; `crates/brgr-store/src/lib.rs:738` (decision plus ack transaction) | `crates/brgr-cli/tests/cli_contract.rs:81`, `crates/brgr-store/src/lib.rs:1438`, `:1678`, `:1739`; reject smoke below |

## Independent reject smoke

Built the CLI with `cargo build -p brgr-cli --locked` (exit 0), then used the repository's disposable `testdata/fixtures/gjc` executable with `brgr --home /tmp/brgr-four-stage.zHLSBH/home --json`. In `/tmp/brgr-four-stage.zHLSBH/work`, `harness add`, foreground `run`, `result`, and `reject --reason 'fixture result intentionally rejected'` all exited 0 under owner `codex:evidence`.

- Task `35819800-cd75-4636-9a9a-46b148347340`, revision 1, returned `candidate`. `result` returned text `BRGR_FIXTURE_OK` and result ID `809ab717-30c4-41b6-b86d-c4ebed2b33ea`.
- The sealed 15-byte artifact reference had digest `sha256:770fc6713b7be966375c363b51c1fe2ccab11612c89fb9e987089fd013f57504`; independent `shasum -a 256` of the stored file matched.
- The CLI returned `verdict: rejected`. A separate read-only SQLite query of the smoke store found **1 result, 1 inbox item, 1 decision, acknowledged=1, verdict=rejected**. The disposable `/tmp/brgr-four-stage.zHLSBH` directory was retained, not deleted.

## Evidence boundary

This re-runs deterministic fixture tests and one local fixture CLI smoke. It does **not** prove a live GJC/OMP/Cursor/Command Code model run, natural-language Codex acceptance judgment, OMP callback migration/cancellation, crash-window completeness, or public release readiness. No source implementation, PR, tag, or release was changed by this verification.
