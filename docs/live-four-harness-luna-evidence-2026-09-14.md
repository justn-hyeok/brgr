# Four real-harness Luna runs — 2026-09-14

Verdict: **four local managed Luna smoke runs passed**, one each through GJC, OMP, Cursor CLI, and Command Code. This is not a v1 release or final-PR-SHA certification.

## Provenance and method

- Coordinator worktree: `/Users/justn/dev/.worktrees/brgr-v1`, branch `loop/brgr-v1`, tracked HEAD `64119cbe56b0b955aadf59b3990780be00806cf0`. Each of the four fresh task worktrees also resolved to that HEAD. `--allow-clean-head-snapshot` explicitly excluded the coordinator's pre-existing uncommitted documents and subsequent local adapter edits; no tracked files changed in the task worktrees. GJC and OMP left only their own untracked `.gjc/` and `.omp-role/` runtime metadata.
- Isolated supervisor home and retained receipts: `/tmp/brgr-live-four.WFsWcN/home`. Owner: `codex:live-four`. The four installed executables were probed and registered in that home; all four `brgr harness status` checks returned `healthy` after the runs. Cursor CLI and Command Code required the new local process recipes in `brgr-registry`; their draft/contract-test and Luna scratch activation passed before task admission.
- The activation receipts in `home/registry/activations/` recorded nonempty scratch-result digests: Cursor CLI `31ba9101bf7843af1ff2a383ac415754cd07c204121c872b8ca49530fcbdfb40`, Command Code `3b5ee6a4994d96b620b7b52d9c2fde08ecf2f3c273eac2ce0aae44d5dee06999`. These receipts establish an output-bearing authorized scratch run, not the correctness of its prose.
- Local `target/debug/brgr` SHA-256: `8ac0d79d45c1dc413b84fe014d0dea00813531329c8ae41d00939676ce1d65a6`. Tracked working diff SHA-256 at smoke time (`git diff --binary | shasum -a 256`): `7bb08af3ad440eba5f7ad6590bef7da03ee5faf78de6d96c8046b04e8cdb54f5`. The Cursor/Command Code recipe and raw-prompt argv support were **uncommitted at the time of the live run**, so [CI run 34750502052](https://github.com/justn-hyeok/brgr/actions/runs/34750502052) at `64119cb` does **not** verify that local binary. Local fmt, strict all-target/all-feature Clippy, and locked all-feature workspace tests passed (56 tests; 0 failed).
- Each model received only a bounded marker request with no tools or file edits requested. OMP used its required report file. Cursor used read-only `ask` mode; Command Code used `plan` mode. No claim is made that these modes are adversarial isolation.

## Result oracle

The table records the model **requested in the immutable brgr task route** and passed through the observed CLI recipe. Except for OMP's launcher selector receipt, a separate native observed-model identity was not persisted; successful explicit selection and response are the available evidence, not proof against an undocumented native fallback.

| Harness / requested Luna | Task and result ID | Sealed reply and artifact SHA-256 | Durable decision |
| --- | --- | --- | --- |
| GJC / `gpt-5.6-luna`, effort `low` | `a6029bca-d42e-4933-ad03-545b3d07542e` / `18cbcdda-bd27-4c6a-b419-18a84aa626c9` | `BRGR_GJC_LUNA_20260914_6D18`; `1fe2a6100e6dc7dcc7ef79742e77478d168fadfa3119df1d66154ad471cff937` | accepted; ack=1 |
| OMP / `openai-codex/gpt-5.6-luna`, effort `low` | `3d606e69-f38e-47fe-9005-03b0f6150b6d` / `80b5bd85-4861-46e3-b1e2-abb15fccf342` | `BRGR_OMP_LUNA_20260914_BA47`; `cf7fe85bd0a6c4243d40c57709e4da1f1aa4257a06301e0e887f51c1da9b9434` | accepted; ack=1; owned pane cleanup `closed` |
| Cursor CLI / `gpt-5.6-luna-low-fast` | `82cee19d-c93b-4c22-b97f-1a395d80d0c4` / `25974fea-e028-45f1-8c54-10705b46111a` | `BRGR_CURSOR_LUNA_MANAGED_20260914_90A3`; `56fe240779230638d3882f678a6d80848933e6cf40b82a1a3795c18a3adc54e8` | accepted; ack=1 |
| Command Code / `gpt-5.6-luna` | `1a844bcc-c3a4-489f-9b49-534e6953c56d` / `64657c77-c530-40b8-b4bd-54003ab2f07f` | `BRGR_COMMANDCODE_LUNA_MANAGED_20260914_17C8`; `5f75694de8491be8141af4f6b9d0d51f4cf0a141d2f5783d05344d9858742b1a` | accepted; ack=1 |

For **each** task, `brgr result` returned `candidate` and the expected marker. Independent `shasum -a 256` of the stored artifact matched its `ArtifactRef.digest`. Before acceptance, SQLite showed exactly one unacknowledged owner inbox item for its result. After Codex inspected the marker and digest and called `brgr accept`, a separate SQLite join found one `accepted` decision, `acknowledged=1`, and an exact match between the stored result digest and decision digest. No nonterminal brgr task remained at the end.

## Failed attempts and limits

- Before the user's Luna-only correction, non-Luna exploratory calls were made; they are **excluded** from the four-run verdict and cannot be undone. No non-Luna model was invoked after the correction.
- An initial GJC request for Gemini failed because its provider credential was unavailable; the failed terminal result was acknowledged, not accepted. GJC Luna succeeded. Command Code's first Luna scratch activation failed because its required credential environment variable name was not allowlisted; after adding only `COMMAND_CODE_API_KEY` to that recipe's environment allowlist, the Luna scratch run passed. The credential value was not read or written to this report.
- Cursor/Command Code recipes, `${input.prompt}` one-argv substitution, and `harness activate --model` were local changes at smoke time. Any later commit and CI must be checked by its own exact SHA. The four real runs prove a bounded happy path, not general registration of every future CLI, crash-window behavior, cancellation, in-flight steering, automatic recovery, absence of a native model fallback, or public unsigned release readiness.
